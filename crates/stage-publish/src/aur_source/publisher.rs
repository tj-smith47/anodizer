use anodizer_core::config::AurSourceConfig;
use anodizer_core::context::Context;

use crate::util;

use super::*;

// ---------------------------------------------------------------------------
// AurSourcePublisher — Publisher trait wrapper (Submitter group)
// ---------------------------------------------------------------------------
//
// Submitter-group; upstream-AUR force-push publisher. Distinct from
// [`crate::aur::AurOurPublisher`] in `aur.rs` which is Manager group with
// `git revert`-based rollback against AUR repos we own. This publisher
// covers the **upstream-AUR source-package** flow: it generates a
// PKGBUILD/.SRCINFO and force-pushes them to an AUR git repo
// (`ssh://aur@aur.archlinux.org/<package>.git`). The push is irreversible
// without coordinating with the AUR maintainer, so rollback is
// warn-only.
//
// CREDENTIAL HANDLING: [`AurSourceTarget`] stores no key material. The
// SSH private key / `GIT_SSH_COMMAND` resolved at publish time
// (`cfg.private_key`, `cfg.git_ssh_command`) is irrelevant to a
// warn-only rollback. We only name the env-var scope operators are
// expected to control (`AUR_SSH_KEY write`) — never the resolved
// secret.

// Submitter-group `Publisher` for the upstream-AUR force-push
// source-publishing flow. Wraps both `publish_to_aur_source` (per-crate)
// and `publish_top_level_aur_sources` (top-level `aur_sources:` array).
//
// Disambiguation: this publisher is NOT the same as
// `crate::aur::AurOurPublisher`. That one is Manager group, with a
// `git revert`-based rollback against AUR repos we own. This one is
// Submitter group, force-pushes upstream AUR repos, and has no
// programmatic rollback.
simple_publisher!(
    AurSourcePublisher,
    "upstream-aur",
    anodizer_core::PublisherGroup::Submitter,
    false,
    Some("AUR_SSH_KEY write"),
);

/// Serialized shape of a recorded upstream-AUR force-push target.
///
/// `package` is the resolved AUR package name (post-template, post
/// `-bin` strip when relevant); `tag` is the current
/// [`anodizer_core::context::Context::version`] tag the source archive
/// references. `git_url` is the `ssh://aur@aur.archlinux.org/...`
/// Aliased to the core-owned snapshot so the evidence schema lives in
/// [`anodizer_core::publish_evidence`] and credential-shaped fields
/// (`private_key` / `git_ssh_command`) have no slot to land in. See
/// the Submitter rustdoc above for the credential-handling rationale.
pub(super) type AurSourceTarget = anodizer_core::publish_evidence::AurSourceTargetSnapshot;

/// Decode the `aur_source_targets` array from
/// [`anodizer_core::PublishEvidence::extra`].
pub(super) fn decode_aur_source_targets(
    extra: &anodizer_core::PublishEvidenceExtra,
) -> Vec<AurSourceTarget> {
    match extra {
        anodizer_core::PublishEvidenceExtra::AurSource(a) => a.aur_source_targets.clone(),
        _ => Vec::new(),
    }
}

/// True when at least one crate in the full crate universe has a
/// `publish.aur_source` block OR the top-level `aur_sources:` array is
/// non-empty — the same universe + accessor the per-crate dispatch keys
/// on, so the publisher registers whenever `run()` would emit.
pub(crate) fn is_aur_source_configured(ctx: &Context) -> bool {
    crate::publisher_helpers::is_any_crate_block_configured(ctx, block)
        || crate::publisher_helpers::is_top_level_block_configured(ctx.config.aur_sources.as_ref())
}

/// The crate-level `publish.aur_source` block — the single accessor the
/// registry gate, the gate-override collapse, and the per-crate dispatch
/// predicate all key on.
pub(crate) fn block(
    p: &anodizer_core::config::PublishConfig,
) -> Option<&anodizer_core::config::AurSourceConfig> {
    p.aur_source.as_ref()
}

pub(crate) fn is_aur_source_per_crate_configured(ctx: &Context, crate_name: &str) -> bool {
    crate::publisher_helpers::is_per_crate_block_configured(ctx, crate_name, block)
}

/// Reproduce the AUR-source package-name resolution that
/// `publish_aur_source_entry` uses: explicit `cfg.name` wins, otherwise
/// the default name (crate name for per-crate, project name for top-level)
/// with optional `-bin` stripping.
pub(super) fn resolve_aur_source_package_name(
    cfg: &anodizer_core::config::AurSourceConfig,
    default_name: &str,
    strip_bin_suffix: bool,
) -> String {
    let raw = cfg.name.as_deref().unwrap_or(default_name);
    if strip_bin_suffix {
        raw.strip_suffix("-bin").unwrap_or(raw).to_string()
    } else {
        raw.to_string()
    }
}

/// Resolve the AUR push remote for a source package: an explicit
/// `cfg.git_url` is a verbatim override; otherwise derive the canonical
/// `ssh://aur@aur.archlinux.org/<pkg_name>.git` from the resolved package
/// name, so the push target tracks `pkgbase` and cannot drift.
pub(super) fn aur_source_push_git_url(cfg: &AurSourceConfig, pkg_name: &str) -> String {
    cfg.git_url
        .as_deref()
        .filter(|u| !u.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| util::aur_default_git_url(pkg_name))
}

/// Build an [`AurSourceTarget`] for a single per-crate `aur_source:` block.
pub(super) fn collect_aur_source_per_crate_target(
    ctx: &Context,
    crate_name: &str,
) -> Option<AurSourceTarget> {
    let c = crate::util::find_crate_in_universe(ctx, crate_name)?;
    let cfg = c.publish.as_ref().and_then(|p| p.aur_source.as_ref())?;
    let pkg_name = resolve_aur_source_package_name(cfg, crate_name, false);
    let git_url = aur_source_push_git_url(cfg, &pkg_name);
    Some(AurSourceTarget {
        target: format!("aur_source: crate '{}'", crate_name),
        package: pkg_name,
        tag: ctx.version(),
        git_url,
    })
}

/// Build [`AurSourceTarget`]s for every entry in the top-level
/// `aur_sources:` array.
fn collect_aur_source_top_level_targets(ctx: &Context) -> Vec<AurSourceTarget> {
    let mut out: Vec<AurSourceTarget> = Vec::new();
    let Some(entries) = ctx.config.aur_sources.as_ref() else {
        return out;
    };
    let project_name = ctx
        .template_vars()
        .get("ProjectName")
        .cloned()
        .unwrap_or_default();
    for (i, cfg) in entries.iter().enumerate() {
        let pkg_name = resolve_aur_source_package_name(cfg, &project_name, true);
        let git_url = aur_source_push_git_url(cfg, &pkg_name);
        out.push(AurSourceTarget {
            target: format!("aur_sources[{}]", i),
            package: pkg_name,
            tag: ctx.version(),
            git_url,
        });
    }
    out
}

/// True when at least one aur_source entry — per-crate `publish.aur_source`
/// or top-level `aur_sources:` — evaluates active right now. The per-crate
/// half is additionally scoped to `--crate` / `--all` selection (same
/// semantics as [`crate::publisher_helpers::effective_publish_crates`]:
/// empty selection = every crate; non-empty = exactly those names, so a
/// selected-but-skipped crate cannot masquerade as active via an
/// out-of-scope sibling); the top-level half is project-wide and has no
/// crate to scope against. Shared by
/// [`anodizer_core::Publisher::advisory_requirements`] and
/// [`anodizer_core::Publisher::config_fully_inactive`] so the two cannot
/// diverge. `requirements()` keeps its own filtered iteration (it needs the
/// entries themselves, not just a boolean).
fn any_aur_source_active(ctx: &Context) -> bool {
    let selected = &ctx.options.selected_crates;
    let per_crate_active = ctx
        .config
        .crate_universe()
        .into_iter()
        .filter(|c| selected.is_empty() || selected.iter().any(|s| s == &c.name))
        .filter_map(|c| c.publish.as_ref()?.aur_source.as_ref())
        .any(|a| {
            !crate::publisher_helpers::entry_inactive(
                ctx,
                a.skip.as_ref(),
                a.skip_upload.as_ref(),
                a.if_condition.as_deref(),
            )
        });
    let top_level_active = ctx.config.aur_sources.iter().flatten().any(|a| {
        !crate::publisher_helpers::entry_inactive(
            ctx,
            a.skip.as_ref(),
            a.skip_upload.as_ref(),
            a.if_condition.as_deref(),
        )
    });
    per_crate_active || top_level_active
}

impl anodizer_core::Publisher for AurSourcePublisher {
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
        !any_aur_source_active(ctx)
    }

    fn retain_on_rollback(&self) -> bool {
        Self::resolved_retain_on_rollback(self)
    }

    fn requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        // Both config homes: per-crate `publish.aur_source` and the
        // top-level `aur_sources:` block (the same union
        // `is_aur_source_configured` gates dispatch on). The per-crate half
        // is scoped to `--crate` / `--all` selection, matching
        // `any_aur_source_active`.
        let selected = &ctx.options.selected_crates;
        let per_crate = ctx
            .config
            .crate_universe()
            .into_iter()
            .filter(|c| selected.is_empty() || selected.iter().any(|s| s == &c.name))
            .filter_map(|c| c.publish.as_ref()?.aur_source.as_ref())
            .filter(|a| {
                !crate::publisher_helpers::entry_inactive(
                    ctx,
                    a.skip.as_ref(),
                    a.skip_upload.as_ref(),
                    a.if_condition.as_deref(),
                )
            })
            .flat_map(|a| {
                crate::publisher_helpers::aur_ssh_requirements(
                    a.private_key.as_deref(),
                    a.git_ssh_command.as_deref(),
                )
            });
        let top_level = ctx
            .config
            .aur_sources
            .iter()
            .flatten()
            .filter(|a| {
                !crate::publisher_helpers::entry_inactive(
                    ctx,
                    a.skip.as_ref(),
                    a.skip_upload.as_ref(),
                    a.if_condition.as_deref(),
                )
            })
            .flat_map(|a| {
                crate::publisher_helpers::aur_ssh_requirements(
                    a.private_key.as_deref(),
                    a.git_ssh_command.as_deref(),
                )
            });
        per_crate.chain(top_level).collect()
    }

    fn advisory_requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        // The schema floor's `bash -n` pass over the rendered source
        // PKGBUILD warn+skips when bash is absent — a recommendation, never
        // a gate failure. Same both-homes active-entry gate as
        // `requirements`.
        if !any_aur_source_active(ctx) {
            return Vec::new();
        }
        vec![anodizer_core::EnvRequirement::Tool {
            name: "bash".to_string(),
        }]
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        let log = ctx.logger("publish");
        let mut targets: Vec<AurSourceTarget> = Vec::new();
        let mut any_pushed = false;
        // Implicit-all: when --crate is not passed, walk every crate with a
        // `publish.aur_source` block. Reading `selected_crates` raw here
        // would silently skip per-crate configs — see
        // [`crate::publisher_helpers::effective_publish_crates`].
        let selected = crate::publisher_helpers::effective_publish_crates(
            ctx,
            is_aur_source_per_crate_configured,
        );
        log.status(&crate::publisher_helpers::run_start_message(
            "aur_source",
            selected.len(),
        ));
        // Per-crate aur_source blocks.
        for crate_name in &selected {
            // Defensive guard for explicit `--crate=X` selection when X has
            // no aur_source block; implicit-all is already filtered above.
            if !is_aur_source_per_crate_configured(ctx, crate_name) {
                log.skip_line(
                    ctx.options.show_skipped,
                    &crate::publisher_helpers::no_config_block_message("aur_source", crate_name),
                );
                continue;
            }
            // Re-scope the version/name template vars to THIS crate's own tag so
            // the rendered PKGBUILD `pkgver` — AND the recorded source tag —
            // carry the crate's version, not the first crate's (workspace
            // per-crate independent-version mode). The target snapshot is taken
            // inside the same scope so its recorded `tag` matches what is pushed.
            let (pushed, target) = crate::publisher_helpers::with_published_crate_scope(
                ctx,
                crate_name,
                &anodizer_core::crate_scope::resolve_crate_tag,
                |ctx| {
                    let target = collect_aur_source_per_crate_target(ctx, crate_name);
                    let pushed = publish_to_aur_source(ctx, crate_name, &log)?;
                    Ok((pushed, target))
                },
            )?;
            any_pushed |= pushed;
            if let Some(t) = target {
                targets.push(t);
            }
        }
        // Top-level aur_sources array (project-wide).
        let top_level_targets = collect_aur_source_top_level_targets(ctx);
        if !top_level_targets.is_empty() {
            targets.extend(top_level_targets);
            any_pushed |= publish_top_level_aur_sources(ctx, &log)?;
        }
        if !any_pushed {
            targets.clear();
        }
        let mut evidence = anodizer_core::PublishEvidence::new("upstream-aur");
        if let Some(first) = targets.first() {
            evidence.primary_ref = Some(format!(
                "https://aur.archlinux.org/packages/{}",
                first.package
            ));
        }
        evidence.extra = anodizer_core::PublishEvidenceExtra::AurSource(
            anodizer_core::publish_evidence::AurSourceExtra {
                aur_source_targets: targets,
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
        let targets = decode_aur_source_targets(&evidence.extra);
        if targets.is_empty() {
            log.warn(&crate::publisher_helpers::rollback_empty_warning_msg(
                "upstream-aur",
                "recorded force-pushes",
            ));
            return Ok(());
        }
        for t in &targets {
            log.warn(&format!(
                "upstream-aur force-push to '{}' at tag '{}' is irreversible \
                 without AUR maintainer coordination; verify state at \
                 https://aur.archlinux.org/packages/{} (git URL: {})",
                t.package, t.tag, t.package, t.git_url
            ));
        }
        log.status(&format!(
            "upstream-aur recorded {} force-push(es); irreversible",
            targets.len()
        ));
        Ok(())
    }

    /// Probe AUR maintainer-key reachability before any publisher runs. This
    /// publisher has no companion state-query checker and force-pushes (the
    /// destructive variant), so an unauthorized key is worth surfacing early —
    /// but the SSH handshake is flaky, so a failure warns rather than blocks.
    fn preflight(&self, ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        let per_crate = ctx
            .config
            .crate_universe()
            .into_iter()
            .filter_map(|c| c.publish.as_ref()?.aur_source.as_ref())
            .filter(|a| {
                !crate::publisher_helpers::entry_inactive(
                    ctx,
                    a.skip.as_ref(),
                    a.skip_upload.as_ref(),
                    a.if_condition.as_deref(),
                )
            })
            .map(|a| (a.private_key.as_deref(), a.git_ssh_command.as_deref()));
        let top_level = ctx
            .config
            .aur_sources
            .iter()
            .flatten()
            .filter(|a| {
                !crate::publisher_helpers::entry_inactive(
                    ctx,
                    a.skip.as_ref(),
                    a.skip_upload.as_ref(),
                    a.if_condition.as_deref(),
                )
            })
            .map(|a| (a.private_key.as_deref(), a.git_ssh_command.as_deref()));
        let entries: Vec<_> = per_crate.chain(top_level).collect();
        Ok(crate::aur::aur_ssh_auth_preflight(
            ctx,
            entries,
            "upstream-aur",
        ))
    }
}
