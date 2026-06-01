//! `NpmPublisher` — Manager-group `Publisher` impl wrapping
//! [`publish_to_npm`](super::publish::publish_to_npm).
//!
//! Classification:
//! * **Group**: Manager — like Homebrew, the npm registry is mutable state in
//!   a third-party system (npmjs.org) but is structurally reversible via
//!   `npm unpublish` (within a 72-hour window).
//! * **Required default**: `true` — a failed npm publish is load-bearing for
//!   users who install via `npm i -g`; the operator should know the release is
//!   half-shipped.
//! * **Rollback scope**: `NPM_TOKEN unpublish`.
//!
//! Evidence: one [`NpmTargetSnapshot`] per published package (per-platform
//! packages + the metapackage in optional-deps mode). Skip / dry-run /
//! no-binaries paths produce no evidence.

use anodizer_core::context::Context;

use super::publish::publish_to_npm;

simple_publisher!(
    NpmPublisher,
    "npm",
    anodizer_core::PublisherGroup::Manager,
    true,
    Some("NPM_TOKEN unpublish"),
);

/// Aliased to the core-owned snapshot so the evidence schema lives in
/// [`anodizer_core::publish_evidence`] and credential-shaped fields have no
/// slot to land in.
pub(crate) type NpmTarget = anodizer_core::publish_evidence::NpmTargetSnapshot;

/// Decode the `npm_targets` array from
/// [`anodizer_core::PublishEvidence::extra`]. Rollback treats an empty decode
/// the same as no-evidence.
fn decode_npm_targets(extra: &anodizer_core::PublishEvidenceExtra) -> Vec<NpmTarget> {
    match extra {
        anodizer_core::PublishEvidenceExtra::Npm(n) => n.npm_targets.clone(),
        _ => Vec::new(),
    }
}

impl anodizer_core::Publisher for NpmPublisher {
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

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        let log = ctx.logger("publish");
        let entries = ctx.config.npms.clone().unwrap_or_default();
        if entries.is_empty() {
            log.status("npm: no `npms:` entries configured");
            return Ok(anodizer_core::PublishEvidence::new("npm"));
        }
        log.status(&format!(
            "npm: starting publish for {} entry(ies)",
            entries.len()
        ));

        // Accumulate every package that publishes successfully BEFORE the
        // next attempt, so a mid-sequence failure still yields evidence for
        // the already-live (72h-irreversible) packages. `publish_to_npm`
        // pushes each success into `pushed`; on Err the evidence is built from
        // whatever it managed to push, the Failed outcome is recorded, and
        // `Ok(evidence)` is returned — bubbling `Err` here would make dispatch
        // drop the evidence (`evidence: None`) and orphan the published
        // packages from rollback.
        let mut pushed: Vec<super::publish::NpmTarget> = Vec::new();
        let mut publish_err: Option<anyhow::Error> = None;
        for (idx, cfg) in entries.iter().enumerate() {
            let label = cfg.id.clone().unwrap_or_else(|| format!("npms[{}]", idx));
            log.status(&format!("npm: processing '{}'", label));
            // Per-crate associations are out of scope for the top-level
            // `npms:` block — the first crate name (or the project name) is
            // the package-name fallback for an unnamed entry.
            let crate_name = ctx
                .config
                .crates
                .first()
                .map(|c| c.name.clone())
                .unwrap_or_else(|| ctx.config.project_name.clone());
            if let Err(e) = publish_to_npm(ctx, cfg, &crate_name, &log, &mut pushed) {
                publish_err = Some(e);
                break;
            }
        }

        let targets: Vec<NpmTarget> = pushed
            .into_iter()
            .map(|t| NpmTarget {
                target: t.package.clone(),
                package: t.package,
                version: t.version,
                registry: t.registry,
                dist_tag: t.dist_tag,
                token_env_var: t.token_env_var,
            })
            .collect();

        let mut evidence = anodizer_core::PublishEvidence::new("npm");
        if let Some(first) = targets.first() {
            evidence.primary_ref = Some(format!(
                "{}/{}/{}",
                first.registry.trim_end_matches('/'),
                first.package,
                first.version
            ));
        }
        if !targets.is_empty() {
            evidence.extra = anodizer_core::PublishEvidenceExtra::Npm(
                anodizer_core::publish_evidence::NpmExtra {
                    npm_targets: targets,
                },
            );
        }

        // Record the failure as an outcome override (keeping the evidence)
        // rather than bubbling `Err` so dispatch retains the rollback
        // coordinates of the packages already pushed.
        if let Some(e) = publish_err {
            log.error(&format!("npm: publish failed: {e:#}"));
            ctx.record_publisher_outcome(anodizer_core::PublisherOutcome::Failed(format!("{e:#}")));
        }
        Ok(evidence)
    }

    fn rollback(
        &self,
        ctx: &mut Context,
        evidence: &anodizer_core::PublishEvidence,
    ) -> anyhow::Result<()> {
        let log = ctx.logger("publish");
        let targets = decode_npm_targets(&evidence.extra);
        if targets.is_empty() {
            log.warn(&crate::publisher_helpers::rollback_empty_warning_msg(
                "npm",
                "published packages",
            ));
            return Ok(());
        }

        // For each recorded target, attempt `npm unpublish`. Within the 72h
        // window this succeeds; outside it npm exits non-zero and the caller
        // surfaces a manual-cleanup warning. Failures here are warn-only so
        // sibling publishers' rollback paths still run.
        let env = ctx.env_source();
        let mut succeeded = 0usize;
        let mut failed = 0usize;
        for t in &targets {
            let token = env.var(&t.token_env_var).unwrap_or_default().to_string();
            if token.is_empty() {
                log.warn(&format!(
                    "npm: rollback of '{}@{}' skipped — env var ${} is unset; \
                     manually run `npm unpublish {}@{}` within 72h",
                    t.package, t.version, t.token_env_var, t.package, t.version
                ));
                failed += 1;
                continue;
            }
            let cfg_dir = match tempfile::TempDir::new() {
                Ok(d) => d,
                Err(e) => {
                    log.warn(&format!(
                        "npm: rollback of '{}@{}' could not create .npmrc temp dir ({:#}); \
                         manual cleanup required",
                        t.package, t.version, e
                    ));
                    failed += 1;
                    continue;
                }
            };
            if let Err(e) = super::publish::write_npmrc(cfg_dir.path(), &t.registry, &token, None) {
                log.warn(&format!(
                    "npm: rollback of '{}@{}' could not write .npmrc ({:#}); \
                     manual cleanup required",
                    t.package, t.version, e
                ));
                failed += 1;
                continue;
            }
            match super::publish::run_npm_unpublish(
                &t.package,
                &t.version,
                cfg_dir.path(),
                &t.registry,
                &log,
            ) {
                Ok(()) => {
                    log.status(&format!("npm: unpublished '{}@{}'", t.package, t.version));
                    succeeded += 1;
                }
                Err(e) => {
                    log.warn(&format!(
                        "npm: failed to unpublish '{}@{}' ({:#}); \
                         after 72h npm no longer permits unpublish — manual \
                         deprecation may be the only remediation",
                        t.package, t.version, e
                    ));
                    failed += 1;
                }
            }
        }
        log.status(&format!(
            "npm: rollback complete — {} unpublished, {} failure(s)",
            succeeded, failed
        ));
        Ok(())
    }

    fn preflight(&self, _ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        Ok(anodizer_core::PreflightCheck::Pass)
    }
}
