//! `GemFuryPublisher` — Manager-group `Publisher` impl wrapping
//! [`publish_to_gemfury`](super::publish::publish_to_gemfury).
//!
//! Classification:
//! * **Group**: Manager — Fury repositories are mutable state in a
//!   third-party system but are programmatically reversible via the
//!   per-version delete API.
//! * **Required default**: `true` — a failed Fury push is load-bearing
//!   for users who install via `apt-get` / `dnf` / `apk` against the
//!   Fury repo; the operator should know the release is half-shipped.
//! * **Rollback scope**: `FURY_API_TOKEN delete` — the env var the
//!   rollback path consults to re-authenticate against the API.
//!
//! Evidence: one [`GemFuryTargetSnapshot`] per artifact actually pushed
//! (skip / dry-run / `if` falsy / idempotent paths produce no entry).

use std::time::Duration;

use anodizer_core::context::Context;

use super::publish::{
    GemFuryTarget, api_token_env_var, delete_version, publish_to_gemfury, resolve_api_token,
};

simple_publisher!(
    GemFuryPublisher,
    "gemfury",
    anodizer_core::PublisherGroup::Manager,
    true,
    Some("FURY_API_TOKEN delete"),
);

/// Aliased to the core-owned snapshot so the evidence schema lives in
/// [`anodizer_core::publish_evidence`] and credential-shaped fields
/// have no slot to land in.
pub(crate) type GemFurySnapshot = anodizer_core::publish_evidence::GemFuryTargetSnapshot;

/// Decode the `gemfury_targets` array from
/// [`anodizer_core::PublishEvidence::extra`]. Rollback treats an empty
/// decode the same as no-evidence and emits the canonical empty-evidence
/// warn.
fn decode_gemfury_targets(extra: &anodizer_core::PublishEvidenceExtra) -> Vec<GemFurySnapshot> {
    match extra {
        anodizer_core::PublishEvidenceExtra::GemFury(g) => g.gemfury_targets.clone(),
        _ => Vec::new(),
    }
}

/// Encode the per-target structs into the typed
/// [`PublishEvidenceExtra::GemFury`] variant.
fn encode_gemfury_targets(targets: &[GemFuryTarget]) -> anodizer_core::PublishEvidenceExtra {
    let snapshots: Vec<GemFurySnapshot> = targets
        .iter()
        .map(|t| GemFurySnapshot {
            target: format!("{}/{}", t.account, t.package),
            account: t.account.clone(),
            package: t.package.clone(),
            version: t.version.clone(),
            format: t.format.clone(),
            push_token_env_var: t.push_token_env_var.clone(),
            api_token_env_var: t.api_token_env_var.clone(),
        })
        .collect();
    anodizer_core::PublishEvidenceExtra::GemFury(anodizer_core::publish_evidence::GemFuryExtra {
        gemfury_targets: snapshots,
    })
}

/// Delete a set of landed GemFury targets via the per-version API.
///
/// Shared by [`GemFuryPublisher::rollback`] (post-publish rollback from
/// recorded evidence) and the in-run partial-push cleanup, so a failure
/// after some artifacts landed undoes exactly what this run placed. Best
/// effort: a delete that fails (or a missing token) is warned, not raised —
/// the goal is to remove as much of the half-landed push as possible while
/// surfacing whatever the operator must clean up by hand.
fn delete_recorded_targets(ctx: &mut Context, targets: &[GemFuryTarget]) {
    let log = ctx.logger("publish");
    let client = match anodizer_core::http::blocking_client(Duration::from_secs(30)) {
        Ok(c) => c,
        Err(e) => {
            log.warn(&format!(
                "gemfury: could not build HTTP client for rollback ({:#}); \
                 manual cleanup required via the GemFury dashboard",
                e
            ));
            return;
        }
    };
    let policy = ctx.retry_policy();

    // Find the per-entry API-token override (if any) for each target. The
    // target carries only the env-var NAME — re-read the config to honor a
    // `cfg.api_token` override that wasn't present in the env at rollback
    // time. When the config no longer declares the entry (config was edited
    // between runs), fall back to the env-var only path.
    let cfg_entries = ctx.config.gemfury.clone().unwrap_or_default();

    let mut deleted = 0usize;
    let mut failed = 0usize;
    let mut warn_only = 0usize;
    for t in targets {
        // Prefer the cfg-supplied api_token when an entry declares one for
        // the same env-var name. This lets a rollback succeed even when the
        // operator's shell doesn't currently export the env var but the
        // config (rendered through the template engine) still resolves it.
        let cfg_token = cfg_entries
            .iter()
            .find(|c| api_token_env_var(c) == t.api_token_env_var)
            .and_then(|c| match resolve_api_token(ctx, c) {
                Ok(s) if !s.is_empty() => Some(s),
                _ => None,
            });
        let env_token = ctx.env_var(&t.api_token_env_var).unwrap_or_default();
        let token = cfg_token.unwrap_or(env_token);
        if token.is_empty() {
            log.warn(&format!(
                "gemfury: rollback of '{}/{}@{}' skipped — ${} not set and no \
                 `api_token` configured; manually delete via the GemFury dashboard",
                t.account, t.package, t.version, t.api_token_env_var
            ));
            warn_only += 1;
            continue;
        }
        match delete_version(
            &client, &t.account, &t.package, &t.version, &token, &policy, &log,
        ) {
            Ok(()) => {
                log.status(&format!(
                    "gemfury: deleted '{}/{}@{}'",
                    t.account, t.package, t.version
                ));
                deleted += 1;
            }
            Err(e) => {
                log.warn(&format!(
                    "gemfury: failed to delete '{}/{}@{}' ({:#}); manual cleanup required",
                    t.account, t.package, t.version, e
                ));
                failed += 1;
            }
        }
    }
    log.status(&format!(
        "gemfury: rollback complete — {} deleted, {} warn-only, {} failure(s)",
        deleted, warn_only, failed
    ));
}

impl anodizer_core::Publisher for GemFuryPublisher {
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
        // `pushed` accumulates landed artifacts. On a mid-loop failure it
        // holds the partial set — the artifacts that DID land before the
        // error. The dispatch layer records NO evidence on an `Err` return
        // (it can only carry evidence on `Ok`), so those partials would
        // otherwise be orphaned: a required-publisher failure aborts the
        // release without ever deleting what gemfury already pushed. Roll
        // the partials back in-place here, before re-raising, so a failed
        // push leaves no half-landed packages on the Fury repo.
        let mut pushed: Vec<GemFuryTarget> = Vec::new();
        if let Err(err) = publish_to_gemfury(ctx, &log, &mut pushed) {
            if !pushed.is_empty() {
                log.warn(&format!(
                    "gemfury: push failed after {} artifact(s) landed — rolling back \
                     the partial push before failing the release",
                    pushed.len()
                ));
                delete_recorded_targets(ctx, &pushed);
            }
            return Err(err);
        }

        let mut evidence = anodizer_core::PublishEvidence::new("gemfury");
        if let Some(first) = pushed.first() {
            evidence.primary_ref = Some(format!(
                "{}/{}@{}",
                first.account, first.package, first.version
            ));
        }
        if !pushed.is_empty() {
            evidence.extra = encode_gemfury_targets(&pushed);
        }
        Ok(evidence)
    }

    fn rollback(
        &self,
        ctx: &mut Context,
        evidence: &anodizer_core::PublishEvidence,
    ) -> anyhow::Result<()> {
        let log = ctx.logger("publish");
        let snapshots = decode_gemfury_targets(&evidence.extra);
        if snapshots.is_empty() {
            log.warn(&crate::publisher_helpers::rollback_empty_warning_msg(
                "gemfury",
                "pushed packages",
            ));
            return Ok(());
        }
        // The evidence snapshot and the live `GemFuryTarget` carry the same
        // delete-keying fields; map onto the shared deletion helper so the
        // post-publish rollback and the in-run partial cleanup share one path.
        let targets: Vec<GemFuryTarget> = snapshots
            .iter()
            .map(|s| GemFuryTarget {
                account: s.account.clone(),
                package: s.package.clone(),
                version: s.version.clone(),
                format: s.format.clone(),
                push_token_env_var: s.push_token_env_var.clone(),
                api_token_env_var: s.api_token_env_var.clone(),
            })
            .collect();
        delete_recorded_targets(ctx, &targets);
        Ok(())
    }

    fn preflight(&self, _ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        Ok(anodizer_core::PreflightCheck::Pass)
    }
}
