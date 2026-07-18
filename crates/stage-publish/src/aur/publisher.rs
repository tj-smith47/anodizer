use super::*;

// ---------------------------------------------------------------------------
// AurOurPublisher — Publisher trait wrapper (git-revert rollback)
// ---------------------------------------------------------------------------

/// `Publisher` for the AUR repo we own (the per-crate
/// `publish.aur:` entry that pushes a binary PKGBUILD to a dedicated
/// AUR package we control via SSH).
///
/// Named `AurOurPublisher` to disambiguate from the upstream-AUR
/// force-push publisher (`aur_source:`) — that one is Submitter group,
/// has no rollback path (irreversible force-push), and writes to
/// packages we do NOT own.
///
/// Rollback shape mirrors the other git-revert publishers: re-clone
/// via the configured SSH key + command, run `git revert HEAD --no-edit`,
/// push to `master` (AUR's only branch).
///
/// SECURITY NOTE: [`AurOurTarget`]'s SSH credentials (`private_key`,
/// `git_ssh_command`) carry `#[serde(skip)]` so they never land in
/// persisted evidence (`dist/run-<id>/report.json`, the run-summary
/// JSON, or the announce-time release-body summary). Rollback
/// re-reads them from the live `ctx.config` at yank time so a
/// rotated SSH key is correctly picked up; if the user rotated and
/// the new key lacks AUR push access, the failure surfaces clearly
/// in the per-target warn line.
use crate::util::{RevertTarget, run_revert_targets_parallel};
use serde::{Deserialize, Serialize};

/// AUR has a single branch convention: every package repo lives on
/// `master`. Pinning this in one constant means both the publish path
/// and the rollback path push to the same name and a future drift
/// (e.g. a stray rename to `main`) is impossible without editing this
/// line.
pub(crate) const AUR_REPO_BRANCH: &str = "master";

simple_publisher!(
    AurOurPublisher,
    "aur",
    anodizer_core::PublisherGroup::Manager,
    false,
    Some("AUR_SSH_KEY write"),
);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct AurOurTarget {
    pub(crate) target: String,
    /// AUR SSH URL — typically
    /// `ssh://aur@aur.archlinux.org/<package>.git`.
    pub(crate) git_url: String,
    /// Inline SSH private-key contents. Captured at run-time from the
    /// active `aur.private_key:` config so a same-process rollback
    /// doesn't have to re-read config; but `#[serde(skip)]` keeps it
    /// out of any persisted shape of [`anodizer_core::PublishEvidence`].
    /// When `decode_aur_our_targets` re-hydrates from a previously
    /// serialized evidence blob this field comes back as `None` and
    /// [`AurOurPublisher::rollback`] re-resolves it from
    /// `ctx.config.crates[*].publish.aur.private_key` by matching
    /// `git_url`.
    ///
    /// SECURITY: persistence tasks (`--rollback-only --from-run`,
    /// `--summary-json`, the announce-time release-body summary) all
    /// round-trip evidence through serde JSON; `#[serde(skip)]` is
    /// the single point of control that keeps the SSH key from
    /// leaking through any of them.
    #[serde(skip)]
    pub(crate) private_key: Option<String>,
    /// Custom `GIT_SSH_COMMAND` override (alternative to
    /// `private_key` — same precedence the publish path uses).
    /// Same `#[serde(skip)]` rationale as `private_key`: the command
    /// can reference an on-disk key path that we treat as
    /// secret-sensitive.
    #[serde(skip)]
    pub(crate) git_ssh_command: Option<String>,
}

/// Walk the crate universe for a `publish.aur` block whose `git_url`
/// matches `git_url` and return the resolved
/// `(private_key, git_ssh_command)` pair. Used at rollback time so
/// the SSH credentials never need to round-trip through serialized
/// evidence.
///
/// Returns `(None, None)` when no crate is configured for the given
/// URL — the rollback `git push` will fail loudly via the warning
/// helper that points the operator at `publish.aur.private_key`.
pub(crate) fn resolve_aur_credentials_from_config(
    ctx: &Context,
    git_url: &str,
) -> anyhow::Result<(Option<String>, Option<String>)> {
    for c in ctx.config.crate_universe() {
        let Some(ac) = c.publish.as_ref().and_then(|p| p.aur.as_ref()) else {
            continue;
        };
        if ac.git_url.as_deref() == Some(git_url) {
            // Render the SSH credentials before they reach the rollback
            // clone, or a templated `{{ .Env.AUR_SSH_KEY }}` lands as the
            // literal string in the key file and ssh fails.
            let pk = ac
                .private_key
                .as_deref()
                .map(|v| {
                    ctx.render_template(v)
                        .with_context(|| format!("aur: render private_key template {v:?}"))
                })
                .transpose()?;
            let ssh = ac
                .git_ssh_command
                .as_deref()
                .map(|v| {
                    ctx.render_template(v)
                        .with_context(|| format!("aur: render git_ssh_command template {v:?}"))
                })
                .transpose()?;
            return Ok((pk, ssh));
        }
    }
    Ok((None, None))
}

/// Collapse the recorded rollback targets to a unique set keyed by
/// `git_url` (AUR always pushes to `master`, so branch is implicit).
///
/// The first entry seen for a given `git_url` wins; later entries that
/// share the same URL are dropped because the second `git revert HEAD`
/// against the same repo would revert the first revert and restore
/// the bad release.
pub(crate) fn dedup_aur_targets(targets: &[AurOurTarget]) -> Vec<AurOurTarget> {
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut out: Vec<AurOurTarget> = Vec::with_capacity(targets.len());
    for t in targets {
        if seen.insert(t.git_url.clone()) {
            out.push(t.clone());
        }
    }
    out
}

impl From<&AurOurTarget> for anodizer_core::publish_evidence::AurTargetSnapshot {
    fn from(t: &AurOurTarget) -> Self {
        Self {
            target: t.target.clone(),
            git_url: t.git_url.clone(),
        }
    }
}

impl From<anodizer_core::publish_evidence::AurTargetSnapshot> for AurOurTarget {
    fn from(s: anodizer_core::publish_evidence::AurTargetSnapshot) -> Self {
        Self {
            target: s.target,
            git_url: s.git_url,
            // SSH credentials are NOT carried in the snapshot — they
            // live only in the live `aur.private_key:` config and are
            // resolved at rollback time via
            // `resolve_aur_credentials_from_config`. This decode
            // boundary matches what the prior `#[serde(skip)]` shape
            // produced when the serialized evidence round-tripped.
            private_key: None,
            git_ssh_command: None,
        }
    }
}

pub(crate) fn decode_aur_our_targets(
    extra: &anodizer_core::PublishEvidenceExtra,
) -> Vec<AurOurTarget> {
    match extra {
        anodizer_core::PublishEvidenceExtra::Aur(a) => {
            a.aur_our_targets.iter().cloned().map(Into::into).collect()
        }
        _ => Vec::new(),
    }
}

pub(crate) fn collect_aur_our_run_targets(
    ctx: &Context,
    log: &StageLogger,
) -> Result<Vec<AurOurTarget>> {
    let mut out: Vec<AurOurTarget> = Vec::new();
    let selected = &ctx.options.selected_crates;
    for c in ctx.config.crate_universe() {
        if !selected.is_empty() && !selected.contains(&c.name) {
            continue;
        }
        let Some(ac) = c.publish.as_ref().and_then(|p| p.aur.as_ref()) else {
            continue;
        };
        // Record the exact remote the live push resolves to (explicit
        // override, else the canonical derived url) so the rollback target
        // never drifts from the pushed repo. Reuses the live-push resolver
        // as the single source of truth.
        let git_url = aur_resolve_push_git_url(ctx, ac, &c.name, log)?;
        // Use the package name (or the AUR-default of `<crate>-bin`)
        // as the human label so log lines say what was rolled back.
        let raw_pkg = aur_default_package_name(ac, &c.name);
        let label = util::render_or_warn(ctx, log, "aur.name", &raw_pkg)?;
        // Render the SSH credentials at collect-time so the recorded
        // rollback target carries the resolved secret, never a literal
        // `{{ .Env.AUR_SSH_KEY }}` that would fail ssh at revert time.
        let private_key = match ac.private_key.as_deref() {
            Some(pk) => Some(util::render_or_warn(ctx, log, "aur.private_key", pk)?),
            None => None,
        };
        let git_ssh_command = match ac.git_ssh_command.as_deref() {
            Some(sc) => Some(util::render_or_warn(ctx, log, "aur.git_ssh_command", sc)?),
            None => None,
        };
        out.push(AurOurTarget {
            target: label,
            git_url,
            private_key,
            git_ssh_command,
        });
    }
    Ok(out)
}

/// The crate-level `publish.aur` block — the single accessor the
/// registry gate, the gate-override collapse, and the per-crate dispatch
/// predicate all key on.
pub(crate) fn block(
    p: &anodizer_core::config::PublishConfig,
) -> Option<&anodizer_core::config::AurConfig> {
    p.aur.as_ref()
}

pub(crate) fn is_aur_per_crate_configured(ctx: &Context, crate_name: &str) -> bool {
    crate::publisher_helpers::is_per_crate_block_configured(ctx, crate_name, block)
}

/// Message emitted just before delegating to `publish_to_aur`. Anchors the
/// AUR activity (PKGBUILD render, git clone, push) to a specific crate in
/// the log so multi-crate workspaces are disambiguatable.
pub(crate) fn run_per_crate_start_message(crate_name: &str) -> String {
    format!("starting per-crate aur publish for '{}'", crate_name)
}

/// Final summary emitted at publisher exit. `processed` is the count of
/// crates the publisher actually invoked `publish_to_aur` on (not the
/// count of successful AUR pushes — `publish_to_aur` has its own skip
/// paths for skip_upload/dry-run/etc., each of which logs its own status
/// line).
pub(crate) fn run_done_message(processed: usize) -> String {
    format!(
        "finished aur publish — {} configured crate(s) processed",
        processed
    )
}

/// Decision predicate for the no-eligible-crates warning. True when the
/// publisher walked the selection but the configured-predicate filtered
/// every crate out — distinct from "ran successfully in dry-run mode".
///
/// `processed` is the count of crates whose `is_aur_per_crate_configured`
/// check passed and whose `publish_to_aur` invocation was reached.
/// `selected_len` is the size of the implicit-all-resolved selection.
pub(crate) fn should_warn_no_eligible(processed: usize, selected_len: usize) -> bool {
    processed == 0 && selected_len > 0
}

/// Warning emitted when the publisher was registered (at least one crate
/// has a `publish.aur` block at the config level) but the run path
/// processed zero crates.
///
/// With the implicit-all default in
/// [`crate::publisher_helpers::effective_publish_crates`], an empty
/// `selected_crates` resolves to every crate carrying a `publish.aur`
/// block — so a zero-processed run means `--crate`/`--all` matrix
/// selection was non-empty AND filtered every aur-configured crate out.
/// Operators must see this — otherwise the publisher's `succeeded` status
/// hides the fact that nothing was pushed.
pub(crate) fn run_no_eligible_crates_warning(selected_total: usize) -> String {
    format!(
        "aur publisher registered but 0 of {} effective crate(s) had an aur \
         config block — nothing pushed. Check that --crate / --all selects a \
         crate whose publish.aur block is set.",
        selected_total
    )
}

/// Aur (Manager-group) entries across the crate universe whose
/// `skip:`/`skip_upload:`/`if:` evaluates active right now AND whose crate
/// is in scope for `--crate` / `--all` selection (same semantics as
/// [`crate::publisher_helpers::effective_publish_crates`]: empty selection
/// = every crate; non-empty = exactly those names, so a selected-but-skipped
/// crate cannot masquerade as active via an out-of-scope sibling). Shared by
/// [`anodizer_core::Publisher::requirements`],
/// [`anodizer_core::Publisher::advisory_requirements`], and
/// [`anodizer_core::Publisher::config_fully_inactive`] so the active-entry
/// gate cannot diverge across the three call sites.
pub(crate) fn active_aur_configs(ctx: &Context) -> Vec<&anodizer_core::config::AurConfig> {
    let selected = &ctx.options.selected_crates;
    ctx.config
        .crate_universe()
        .into_iter()
        .filter(|c| selected.is_empty() || selected.iter().any(|s| s == &c.name))
        .filter_map(|c| c.publish.as_ref()?.aur.as_ref())
        .filter(|a| {
            !crate::publisher_helpers::entry_inactive(
                ctx,
                a.skip.as_ref(),
                a.skip_upload.as_ref(),
                a.if_condition.as_deref(),
            )
        })
        .collect()
}

impl anodizer_core::Publisher for AurOurPublisher {
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

    fn config_fully_inactive(&self, ctx: &Context) -> bool {
        active_aur_configs(ctx).is_empty()
    }

    fn requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        active_aur_configs(ctx)
            .into_iter()
            .flat_map(|a| {
                crate::publisher_helpers::aur_ssh_requirements(
                    a.private_key.as_deref(),
                    a.git_ssh_command.as_deref(),
                )
            })
            .collect()
    }

    fn advisory_requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        // The schema floor's `bash -n` pass over the rendered PKGBUILD
        // warn+skips when bash is absent — a recommendation, never a gate
        // failure. Same active-entry gate as `requirements`.
        if active_aur_configs(ctx).is_empty() {
            return Vec::new();
        }
        vec![anodizer_core::EnvRequirement::Tool {
            name: "bash".to_string(),
        }]
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        let log = ctx.logger("publish");
        let selected =
            crate::publisher_helpers::effective_publish_crates(ctx, is_aur_per_crate_configured);
        log.status(&crate::publisher_helpers::run_start_message(
            "aur",
            selected.len(),
        ));
        let mut processed = 0usize;
        let mut any_pushed = false;
        for crate_name in &selected {
            // Defensive guard for explicit `--crate=X` selection when X has no
            // publisher block; implicit-all is already filtered by effective_publish_crates above.
            if !is_aur_per_crate_configured(ctx, crate_name) {
                log.skip_line(
                    ctx.options.show_skipped,
                    &crate::publisher_helpers::no_config_block_message("aur", crate_name),
                );
                continue;
            }
            processed += 1;
            log.verbose(&run_per_crate_start_message(crate_name));
            // Re-scope the version/name template vars to THIS crate's own tag so
            // the rendered PKGBUILD `pkgver` carries the crate's version, not the
            // first crate's (workspace per-crate independent-version mode).
            let pushed = crate::publisher_helpers::with_published_crate_scope(
                ctx,
                crate_name,
                &anodizer_core::crate_scope::resolve_crate_tag,
                |ctx| publish_to_aur(ctx, crate_name, &log),
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
        let mut evidence = anodizer_core::PublishEvidence::new("aur");
        // Only record rollback targets when at least one push was made.
        // Phantom evidence causes rollback to git-revert in repos that
        // were never touched (dry-run, skip_upload, no-op NoChanges).
        if any_pushed {
            let targets = collect_aur_our_run_targets(ctx, &log)?;
            evidence.extra = anodizer_core::PublishEvidenceExtra::Aur(
                anodizer_core::publish_evidence::AurExtra {
                    aur_our_targets: targets.iter().map(Into::into).collect(),
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
        let log = ctx.logger("publish");
        let targets = decode_aur_our_targets(&evidence.extra);
        if targets.is_empty() {
            log.warn(&crate::publisher_helpers::rollback_empty_warning_msg(
                "aur",
                "AUR repo clone targets",
            ));
            return Ok(());
        }
        // Dedup recorded targets by `(git_url, AUR_REPO_BRANCH)` before
        // fanning out. When two crates share the same AUR repo
        // (unusual for binary PKGBUILDs but possible if a workspace
        // packages multiple binaries into one repo), running `git
        // revert HEAD` twice would revert the first revert — restoring
        // the bad release. Keep the first-seen entry's label so the
        // warn lines still name a meaningful target.
        let unique = dedup_aur_targets(&targets);
        // SSH credentials are not in the serialized evidence
        // (`#[serde(skip)]`). Resolve them from the live config now
        // so the parallel workers each have their own clone of the
        // credential bundle.
        let prepared: Vec<RevertTarget> = unique
            .iter()
            .map(|t| -> anyhow::Result<RevertTarget> {
                let (pk, ssh_cmd) = resolve_aur_credentials_from_config(ctx, &t.git_url)?;
                Ok(RevertTarget {
                    target: t.target.clone(),
                    repo_url: t.git_url.clone(),
                    branch: Some(AUR_REPO_BRANCH.to_string()),
                    token: None,
                    private_key: pk,
                    ssh_command: ssh_cmd,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let (reverted, failed) = run_revert_targets_parallel(&prepared, "aur", None, &log);
        log.status(&format!(
            "aur rollback reverted {} repo(s), {} failure(s)",
            reverted, failed
        ));
        // See the matching comment in `run_token_revert_rollback`: without
        // this, a failed AUR revert is silently folded into the outer
        // `rollback complete` summary's success count instead of `failed`.
        if failed > 0 {
            anyhow::bail!(
                "aur rollback: {failed} of {} repo(s) failed to revert (see per-target warnings above)",
                prepared.len()
            );
        }
        Ok(())
    }

    /// Probe AUR maintainer-key reachability before any publisher runs. The
    /// `requirements()` check only proves the key *parses*; this proves the
    /// remote *accepts* it. Warning-only: an overwritable git push is
    /// recoverable and an SSH handshake is flaky.
    fn preflight(&self, ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        let entries = ctx
            .config
            .crate_universe()
            .into_iter()
            .filter_map(|c| c.publish.as_ref()?.aur.as_ref())
            .filter(|a| {
                !crate::publisher_helpers::entry_inactive(
                    ctx,
                    a.skip.as_ref(),
                    a.skip_upload.as_ref(),
                    a.if_condition.as_deref(),
                )
            })
            .map(|a| (a.private_key.as_deref(), a.git_ssh_command.as_deref()))
            .collect::<Vec<_>>();
        Ok(aur_ssh_auth_preflight(ctx, entries, "aur"))
    }
}

/// AUR host the maintainer SSH key authenticates against.
pub(crate) const AUR_SSH_USER_HOST: &str = "aur@aur.archlinux.org";

/// Best-effort SSH-auth preflight shared by the `aur` and `upstream-aur`
/// publishers. Renders each entry's `(private_key, git_ssh_command)`, dedups by
/// rendered key so a shared maintainer key handshakes once, and probes
/// `aur@aur.archlinux.org`. Always degrades a failure to
/// [`PreflightCheck::Warning`](anodizer_core::PreflightCheck) — never a hard
/// block.
pub(crate) fn aur_ssh_auth_preflight(
    ctx: &Context,
    entries: Vec<(Option<&str>, Option<&str>)>,
    publisher: &str,
) -> anodizer_core::PreflightCheck {
    let log = ctx.logger("preflight");
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut acc = anodizer_core::PreflightCheck::Pass;
    for (pk, ssh_cmd) in entries {
        // A render failure is config-validation territory (surfaced by the
        // publish path's own strict render); skip rather than warn here.
        let Some(raw_key) = pk else { continue };
        let Ok(key) = ctx.render_template(raw_key) else {
            continue;
        };
        if key.is_empty() || !seen.insert(key.clone()) {
            continue;
        }
        let rendered_ssh = ssh_cmd.and_then(|c| ctx.render_template(c).ok());
        if let Err(e) = crate::util::ssh_auth_probe(
            AUR_SSH_USER_HOST,
            Some(&key),
            rendered_ssh.as_deref(),
            &format!("preflight: {publisher} ssh"),
            &log,
        ) {
            log.verbose(&format!("{publisher} ssh auth probe failed: {e}"));
            acc = crate::publisher_preflight::merge(
                acc,
                anodizer_core::PreflightCheck::Warning(format!(
                    "AUR SSH key may not be authorized for {publisher}; the push could fail"
                )),
            );
        }
    }
    acc
}
