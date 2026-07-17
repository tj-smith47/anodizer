//! `NixPublisher` — Manager-group `Publisher` impl that wraps the
//! existing [`publish_to_nix`](super::publish_to_nix) per-crate entry
//! point.
//!
//! Rollback shape mirrors the other git-revert publishers (homebrew /
//! scoop / our-AUR): every push to the configured nix overlay repo is
//! recorded so a `--rollback-only` re-clones, runs `git revert HEAD
//! --no-edit`, and pushes the revert back to the same branch.
//!
//! CREDENTIAL HANDLING: [`NixTarget`] stores `token_env_var` — the
//! NAME of the env var — not the resolved token VALUE. The token is
//! read from the live env at rollback time so persisted evidence
//! carries no secret material. Same rule applies to the homebrew /
//! scoop git-revert publishers.

use anodizer_core::context::Context;

simple_publisher!(
    NixPublisher,
    "nix",
    anodizer_core::PublisherGroup::Manager,
    false,
    Some("GITHUB_TOKEN contents:write"),
);

/// Aliased to the core-owned snapshot so the evidence schema lives in
/// [`anodizer_core::publish_evidence`] and credential-shaped fields
/// have no slot to land in.
type NixTarget = anodizer_core::publish_evidence::NixTargetSnapshot;

fn decode_nix_targets(extra: &anodizer_core::PublishEvidenceExtra) -> Vec<NixTarget> {
    match extra {
        anodizer_core::PublishEvidenceExtra::Nix(n) => n.nix_targets.clone(),
        _ => Vec::new(),
    }
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
    for c in ctx.config.crate_universe() {
        if !selected.is_empty() && !selected.contains(&c.name) {
            continue;
        }
        let Some(nc) = c.publish.as_ref().and_then(|p| p.nix.as_ref()) else {
            continue;
        };
        if let Some((owner, name)) = crate::util::resolve_repo_owner_name(nc.repository.as_ref()) {
            // Mirror the publish path's branch resolution (including the
            // versioned PR-branch default) so the recorded rollback branch
            // matches the branch actually pushed.
            let pkg_raw = nc.name.as_deref().unwrap_or(&c.name);
            let pkg_name = ctx
                .render_template(pkg_raw)
                .unwrap_or_else(|_| pkg_raw.to_string());
            let version = crate::util::crate_scoped_version(ctx, c);
            out.push(NixTarget {
                target: c.name.clone(),
                repo_url: format!("https://github.com/{}/{}.git", owner, name),
                branch: crate::util::resolve_branch_or_versioned(
                    ctx,
                    nc.repository.as_ref(),
                    &pkg_name,
                    &version,
                ),
                token_env_var: Some("NIX_PKGS_TOKEN".to_string()),
            });
        }
    }
    out
}

/// The crate-level `publish.nix` block — the single accessor the
/// registry gate, the gate-override collapse, and the per-crate dispatch
/// predicate all key on.
pub(crate) fn block(
    p: &anodizer_core::config::PublishConfig,
) -> Option<&anodizer_core::config::NixConfig> {
    p.nix.as_ref()
}

pub(crate) fn is_nix_per_crate_configured(ctx: &Context, crate_name: &str) -> bool {
    crate::publisher_helpers::is_per_crate_block_configured(ctx, crate_name, block)
}

/// Message emitted just before delegating to `publish_to_nix`. Anchors
/// the nix activity (overlay derivation render, repo clone, push) to a
/// specific crate in the log so multi-crate workspaces are
/// disambiguatable.
pub(crate) fn run_per_crate_start_message(crate_name: &str) -> String {
    format!("starting per-crate nix publish for '{}'", crate_name)
}

/// Final summary emitted at publisher exit. `processed` is the count of
/// crates the publisher actually invoked `publish_to_nix` on (not the
/// count of successful overlay pushes — `publish_to_nix` has its own
/// skip paths for skip_upload/dry-run/etc., each of which logs its own
/// status line).
pub(crate) fn run_done_message(processed: usize) -> String {
    format!(
        "finished nix publish — {} configured crate(s) processed",
        processed
    )
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
        "nix publisher registered but 0 of {} effective crate(s) had a nix \
         config block — nothing pushed. Check that --crate / --all selects a \
         crate whose publish.nix block is set.",
        selected_total
    )
}

/// Nix entries across the crate universe whose `skip:`/`skip_upload:`/
/// `if:` evaluates active right now AND whose crate is in scope for
/// `--crate` / `--all` selection (same semantics as
/// [`crate::publisher_helpers::effective_publish_crates`]: empty selection
/// = every crate; non-empty = exactly those names, so a selected-but-skipped
/// crate cannot masquerade as active via an out-of-scope sibling). Shared by
/// [`anodizer_core::Publisher::requirements`],
/// [`anodizer_core::Publisher::advisory_requirements`],
/// [`anodizer_core::Publisher::preflight`], and
/// [`anodizer_core::Publisher::config_fully_inactive`] so the active-entry
/// gate cannot diverge across the four call sites.
fn active_nix_configs(ctx: &Context) -> Vec<&anodizer_core::config::NixConfig> {
    let selected = &ctx.options.selected_crates;
    ctx.config
        .crate_universe()
        .into_iter()
        .filter(|c| selected.is_empty() || selected.iter().any(|s| s == &c.name))
        .filter_map(|c| c.publish.as_ref()?.nix.as_ref())
        .filter(|n| {
            !crate::publisher_helpers::entry_inactive(
                ctx,
                n.skip.as_ref(),
                n.skip_upload.as_ref(),
                n.if_condition.as_deref(),
            )
        })
        .collect()
}

impl anodizer_core::Publisher for NixPublisher {
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

    fn config_fully_inactive(&self, ctx: &Context) -> bool {
        active_nix_configs(ctx).is_empty()
    }

    fn retain_on_rollback(&self) -> bool {
        Self::resolved_retain_on_rollback(self)
    }

    fn requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        active_nix_configs(ctx)
            .into_iter()
            .flat_map(|n| {
                let mut reqs = crate::publisher_helpers::git_repo_requirements(
                    ctx,
                    n.repository.as_ref(),
                    Some("NIX_PKGS_TOKEN"),
                );
                // A configured formatter binary (alejandra/nixfmt) is MANDATORY
                // at publish time: `run_formatter` bails if it is absent on
                // PATH, so a clean auto-install runner must provision it. No
                // formatter set is a no-op, matching `run_formatter`. Declared
                // REQUIRED (not advisory) — the caller wraps publisher
                // requirements via `SourcedRequirement::new`, same frame that
                // makes `git`/`npm` hard requirements.
                if let Some(formatter) = n.formatter.as_ref().filter(|f| !f.is_empty()) {
                    reqs.push(anodizer_core::EnvRequirement::Tool {
                        name: formatter.clone(),
                    });
                }
                reqs
            })
            .collect()
    }

    fn advisory_requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        // Same active-entry walk as `requirements`, but for tools whose
        // absence only degrades the publish: the schema floor's
        // `nix-instantiate --parse` pass over the rendered expression
        // warn+skips without the tool, and `gh pr create` is the preferred
        // PR transport with a full REST-API fallback — recommendations,
        // never gate failures.
        let active = active_nix_configs(ctx);
        if active.is_empty() {
            return Vec::new();
        }
        let mut out = vec![anodizer_core::EnvRequirement::Tool {
            name: "nix-instantiate".to_string(),
        }];
        if active
            .iter()
            .any(|n| crate::publisher_helpers::pull_request_enabled(n.repository.as_ref()))
        {
            out.push(anodizer_core::EnvRequirement::Tool {
                name: "gh".to_string(),
            });
        }
        out
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        let log = ctx.logger("publish");
        let selected =
            crate::publisher_helpers::effective_publish_crates(ctx, is_nix_per_crate_configured);
        log.status(&crate::publisher_helpers::run_start_message(
            "nix",
            selected.len(),
        ));
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
                log.skip_line(
                    ctx.options.show_skipped,
                    &crate::publisher_helpers::no_config_block_message("nix", crate_name),
                );
                continue;
            }
            processed += 1;
            log.verbose(&run_per_crate_start_message(crate_name));
            // Re-scope the version/name template vars to THIS crate's own tag so
            // the rendered derivation carries the crate's version, not the first
            // crate's (workspace per-crate independent-version mode).
            let pushed = crate::publisher_helpers::with_published_crate_scope(
                ctx,
                crate_name,
                &anodizer_core::crate_scope::resolve_crate_tag,
                |ctx| super::publish_to_nix(ctx, crate_name, &log),
            )?;
            if pushed {
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
            evidence.extra = anodizer_core::PublishEvidenceExtra::Nix(
                anodizer_core::publish_evidence::NixExtra {
                    nix_targets: targets,
                },
            );
        }
        Ok(evidence)
    }

    fn rollback(
        &self,
        ctx: &mut Context,
        evidence: &anodizer_core::PublishEvidence,
    ) -> anyhow::Result<()> {
        let targets = decode_nix_targets(&evidence.extra);
        let unique = dedup_nix_targets(&targets);
        crate::util::run_token_revert_rollback(
            ctx,
            &unique,
            "nix",
            "NIX_PKGS_TOKEN",
            "overlay clone targets",
            "overlay",
        )
    }

    /// Probe every active overlay repo for existence + push scope before any
    /// publisher runs: a missing overlay or a token without push access fails
    /// the `git push` after sibling publishers may already have shipped.
    fn preflight(&self, ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        // Best-effort pre-publish gate uses the shallow probe policy.
        let policy = anodizer_core::retry::RetryPolicy::PREFLIGHT;
        Ok(crate::publisher_preflight::for_each_active_github_repo(
            ctx,
            &policy,
            "NIX_PKGS_TOKEN",
            active_nix_configs(ctx).into_iter(),
            |_n| true,
            |n| n.repository.as_ref(),
        ))
    }
}

#[cfg(test)]
mod publisher_tests {
    use super::*;
    use anodizer_core::config::{
        CrateConfig, NixConfig, PublishConfig, RepositoryConfig, StringOrBool,
    };
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::{PreflightCheck, PublishEvidence, Publisher, PublisherGroup};

    fn nix_crate(name: &str) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
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

    /// `--crate x` selects only the skip:true entry; an active sibling `y`
    /// outside the selection must not keep the publisher live.
    #[test]
    fn config_fully_inactive_true_when_selected_crate_is_skipped_sibling_active() {
        let mut skipped = nix_crate("x");
        skipped.publish.as_mut().unwrap().nix.as_mut().unwrap().skip =
            Some(StringOrBool::Bool(true));
        let ctx = TestContextBuilder::new()
            .crates(vec![skipped, nix_crate("y")])
            .selected_crates(vec!["x".to_string()])
            .build();

        assert!(
            NixPublisher::new().config_fully_inactive(&ctx),
            "--crate x selects only the skip:true entry; active sibling y is out of \
             scope and must not keep the publisher live"
        );
    }

    /// Empty `--crate` selection means "all crates" — an active entry with
    /// no `--crate` filter applied must keep the publisher live.
    #[test]
    fn config_fully_inactive_false_with_empty_selection_and_active_entry() {
        let ctx = TestContextBuilder::new()
            .crates(vec![nix_crate("x")])
            .build();

        assert!(
            !NixPublisher::new().config_fully_inactive(&ctx),
            "empty selection means \"all crates\"; an active entry must keep the \
             publisher live"
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
        let capture = anodizer_core::log::LogCapture::new();
        let mut ctx = TestContextBuilder::new().build();
        ctx.with_log_capture(capture.clone());
        let evidence = PublishEvidence::new("nix");
        let p = NixPublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());

        let warns = capture.warn_messages();
        assert!(
            warns.iter().any(|m| m.contains("nix")
                && m.contains("overlay clone targets")
                && m.contains("verify")),
            "expected captured warn naming publisher + target-noun + 'verify'; got: {warns:?}"
        );
    }

    #[test]
    fn nix_target_extra_carries_no_secret_material() {
        // Structural pin: build a typed-variant evidence and assert
        // (a) no credential-shaped keys appear AND (b) the
        // operator-public shape is preserved.
        let mut e = PublishEvidence::new("nix");
        e.extra =
            anodizer_core::PublishEvidenceExtra::Nix(anodizer_core::publish_evidence::NixExtra {
                nix_targets: vec![NixTarget {
                    target: "demo".into(),
                    repo_url: "https://github.com/acme/nixpkgs-overlay.git".into(),
                    branch: Some("master".into()),
                    token_env_var: Some("NIX_PKGS_TOKEN".into()),
                }],
            });
        let s = serde_json::to_string(&e).expect("serialize");
        assert!(!s.contains("\"token\":"), "{s}");
        assert!(!s.contains("\"password\":"), "{s}");
        assert!(!s.contains("\"pat\":"), "{s}");
        assert!(!s.contains("\"private_key\":"), "{s}");
        assert!(!s.contains("\"secret\":"), "{s}");
        assert!(!s.contains("\"api_key\":"), "{s}");
        assert!(s.contains("NIX_PKGS_TOKEN"), "{s}");
        assert!(s.contains("\"target\":\"demo\""), "{s}");
        assert!(s.contains("\"branch\":\"master\""), "{s}");
    }

    #[test]
    fn nix_target_extra_roundtrips() {
        let original = vec![NixTarget {
            target: "demo".into(),
            repo_url: "https://github.com/acme/nixpkgs-overlay.git".into(),
            branch: Some("master".into()),
            token_env_var: Some("NIX_PKGS_TOKEN".into()),
        }];
        let extra =
            anodizer_core::PublishEvidenceExtra::Nix(anodizer_core::publish_evidence::NixExtra {
                nix_targets: original.clone(),
            });
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
                    tag_template: Some("v{{ .Version }}".to_string()),
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

    fn nix_crate_with_formatter(name: &str, formatter: Option<&str>) -> CrateConfig {
        let mut c = nix_crate(name);
        if let Some(p) = c.publish.as_mut() {
            if let Some(n) = p.nix.as_mut() {
                n.formatter = formatter.map(str::to_string);
            }
        }
        c
    }

    #[test]
    fn nix_requirements_emit_formatter_tool_when_set() {
        // A configured formatter is mandatory at publish time (run_formatter
        // bails if it is absent), so requirements() must report it — otherwise
        // the action's auto-install (driven by `anodizer tools`) drops it from
        // a clean runner.
        let ctx = TestContextBuilder::new()
            .crates(vec![nix_crate_with_formatter("demo", Some("alejandra"))])
            .build();
        let reqs = NixPublisher::new().requirements(&ctx);
        assert!(
            reqs.iter().any(|r| matches!(
                r,
                anodizer_core::EnvRequirement::Tool { name } if name == "alejandra"
            )),
            "expected a mandatory Tool{{name:\"alejandra\"}} requirement; got: {reqs:?}"
        );
    }

    #[test]
    fn nix_requirements_emit_nixfmt_tool_when_set() {
        let ctx = TestContextBuilder::new()
            .crates(vec![nix_crate_with_formatter("demo", Some("nixfmt"))])
            .build();
        let reqs = NixPublisher::new().requirements(&ctx);
        assert!(
            reqs.iter().any(|r| matches!(
                r,
                anodizer_core::EnvRequirement::Tool { name } if name == "nixfmt"
            )),
            "expected a mandatory Tool{{name:\"nixfmt\"}} requirement; got: {reqs:?}"
        );
    }

    /// The nix schema floor runs `nix-instantiate --parse` over the rendered
    /// expression when the tool is present and warn+skips otherwise, so it is
    /// ADVISORY: recommended to the auto-install layer, never a blocker.
    #[test]
    fn nix_advisory_requirements_emit_nix_instantiate_when_active() {
        let ctx = TestContextBuilder::new()
            .crates(vec![nix_crate("demo")])
            .build();
        let reqs = NixPublisher::new().advisory_requirements(&ctx);
        assert!(
            reqs.iter().any(|r| matches!(
                r,
                anodizer_core::EnvRequirement::Tool { name } if name == "nix-instantiate"
            )),
            "active nix entry must recommend nix-instantiate: {reqs:?}"
        );
        assert!(
            !reqs.iter().any(|r| matches!(
                r,
                anodizer_core::EnvRequirement::Tool { name } if name == "gh"
            )),
            "no pull_request block ⇒ no gh recommendation: {reqs:?}"
        );
    }

    /// `pull_request.enabled` publishes via `gh pr create` when the CLI is
    /// present, with a full REST-API fallback — so `gh` is a recommended
    /// transport, not a hard need.
    #[test]
    fn nix_advisory_requirements_emit_gh_when_pull_request_enabled() {
        let mut c = nix_crate("demo");
        if let Some(repo) = c
            .publish
            .as_mut()
            .and_then(|p| p.nix.as_mut())
            .and_then(|n| n.repository.as_mut())
        {
            repo.pull_request = Some(anodizer_core::config::PullRequestConfig {
                enabled: Some(true),
                ..Default::default()
            });
        }
        let ctx = TestContextBuilder::new().crates(vec![c]).build();
        let reqs = NixPublisher::new().advisory_requirements(&ctx);
        assert!(
            reqs.iter().any(|r| matches!(
                r,
                anodizer_core::EnvRequirement::Tool { name } if name == "gh"
            )),
            "pull_request.enabled must recommend gh: {reqs:?}"
        );
    }

    #[test]
    fn nix_advisory_requirements_empty_when_all_entries_skipped() {
        let mut c = nix_crate("demo");
        if let Some(n) = c.publish.as_mut().and_then(|p| p.nix.as_mut()) {
            n.skip = Some(anodizer_core::config::StringOrBool::Bool(true));
        }
        let ctx = TestContextBuilder::new().crates(vec![c]).build();
        let reqs = NixPublisher::new().advisory_requirements(&ctx);
        assert!(
            reqs.is_empty(),
            "every entry skipped ⇒ no advisory recommendations: {reqs:?}"
        );
    }

    #[test]
    fn nix_requirements_omit_formatter_tool_when_unset() {
        // No formatter configured → no formatter Tool requirement (only the
        // git/token requirements from git_repo_requirements remain). Mirrors
        // run_formatter's opt-in no-op when `formatter` is None.
        let ctx = TestContextBuilder::new()
            .crates(vec![nix_crate_with_formatter("demo", None)])
            .build();
        let reqs = NixPublisher::new().requirements(&ctx);
        assert!(
            !reqs.iter().any(|r| matches!(
                r,
                anodizer_core::EnvRequirement::Tool { name }
                    if name == "alejandra" || name == "nixfmt"
            )),
            "expected NO formatter Tool requirement when formatter unset; got: {reqs:?}"
        );
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
    fn run_per_crate_start_message_names_crate() {
        let msg = run_per_crate_start_message("demo");
        assert!(msg.starts_with("starting per-crate nix publish"), "{msg}");
        assert!(msg.contains("'demo'"), "{msg}");
    }

    #[test]
    fn run_done_message_reports_processed_count() {
        let msg = run_done_message(2);
        assert!(msg.starts_with("finished nix publish"), "{msg}");
        assert!(msg.contains("2 configured crate(s) processed"), "{msg}");
    }

    #[test]
    fn run_no_eligible_crates_warning_names_remediation() {
        let msg = run_no_eligible_crates_warning(5);
        assert!(msg.starts_with("nix publisher registered"), "{msg}");
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
        let repo = crate::testing::hermetic_tagged_repo();
        let mut ctx = TestContextBuilder::new()
            .crates(vec![nix_crate("demo")])
            .selected_crates(vec!["demo".to_string()])
            .dry_run(true)
            .project_root(repo.path().to_path_buf())
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
                    tag_template: Some("v{{ .Version }}".to_string()),
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

    #[test]
    fn nix_publisher_visible_work_contract() {
        use crate::testing::assert_publisher_visible_work_contract;
        let repo = crate::testing::hermetic_tagged_repo();
        let mut ctx = TestContextBuilder::new()
            .crates(vec![nix_crate("demo")])
            .selected_crates(vec!["demo".to_string()])
            .dry_run(true)
            .project_root(repo.path().to_path_buf())
            .build();
        let p = NixPublisher::new();
        assert_publisher_visible_work_contract(&p, &mut ctx);
    }
}
