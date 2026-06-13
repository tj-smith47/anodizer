//! SchemaStore rollback: close the registration PR(s) [`super::publish::run_publish`]
//! opened against `SchemaStore/schemastore`.
//!
//! Mirrors krew's PR-close rollback (`crate::krew::rollback`): decode the
//! recorded PR targets from evidence, dedup, resolve the close-PR token at
//! rollback time (never from evidence), find the open PR number for each
//! `fork_owner:branch` head, and `PATCH state=closed` it. The whole path is
//! best-effort — a per-target failure warns and continues rather than aborting
//! the rollback chain, and the function always returns `Ok(())` (matching
//! krew's contract) so a partial rollback failure never masks the original
//! failure being rolled back.

use anodizer_core::context::Context;
use anodizer_core::{PublishEvidence, PublishEvidenceExtra};

use anodizer_core::publish_evidence::SchemastoreTargetSnapshot;

/// Default env var consulted for the close-PR token when a target recorded
/// none. Kept in sync with the publish path's `TOKEN_ENV_VAR`.
const DEFAULT_TOKEN_ENV_VAR: &str = "SCHEMASTORE_TOKEN";

/// Decode the `schemastore_targets` array from
/// [`anodizer_core::PublishEvidence::extra`]. Returns an empty vec for any
/// other evidence variant (a foreign publisher's evidence, or the empty
/// `PublishEvidence::new("schemastore")` written when no PR was opened).
fn decode_schemastore_targets(extra: &PublishEvidenceExtra) -> Vec<SchemastoreTargetSnapshot> {
    match extra {
        PublishEvidenceExtra::Schemastore(s) => s.schemastore_targets.clone(),
        _ => Vec::new(),
    }
}

/// Collapse targets that name the same PR head. A re-run of the publish path
/// can record the same `(upstream_owner, upstream_repo, fork_owner, branch)`
/// twice; closing the resolved PR once is sufficient, so dedup before any
/// network work. Order of first appearance is preserved.
fn dedup_targets(targets: Vec<SchemastoreTargetSnapshot>) -> Vec<SchemastoreTargetSnapshot> {
    let mut seen: std::collections::BTreeSet<(String, String, String, String)> =
        std::collections::BTreeSet::new();
    let mut out = Vec::with_capacity(targets.len());
    for t in targets {
        let key = (
            t.upstream_owner.clone(),
            t.upstream_repo.clone(),
            t.fork_owner.clone(),
            t.branch.clone(),
        );
        if seen.insert(key) {
            out.push(t);
        }
    }
    out
}

/// Roll back a SchemaStore publish by closing the registration PR(s) it opened.
///
/// Best-effort: closing a PR does not undo a merge, so an already-merged PR
/// (which never appears in the open-PR query) surfaces as an actionable warn
/// telling the operator a manual revert PR is required. Per-target failures
/// (no resolvable token, query error, close error) warn and continue.
pub(crate) fn rollback_publish(
    ctx: &mut Context,
    evidence: &PublishEvidence,
) -> anyhow::Result<()> {
    let log = ctx.logger("publish");
    let targets = dedup_targets(decode_schemastore_targets(&evidence.extra));

    // No targets means the publish opened no PR (every schema already current,
    // dry-run, or the in-flight idempotency path) — nothing to close.
    if targets.is_empty() {
        log.warn(&anodizer_core::rollback_empty_warning_msg(
            "schemastore",
            "PR targets",
        ));
        return Ok(());
    }

    // Resolve the close-PR token at rollback time — evidence never persists it.
    // Falls back to ANODIZER_GITHUB_TOKEN then GITHUB_TOKEN, matching every
    // git-revert publisher.
    let env = ctx.env_source();
    let resolve_token = |t: &SchemastoreTargetSnapshot| -> Option<String> {
        t.token_env_var
            .as_deref()
            .and_then(|n| env.var(n))
            .or_else(|| env.var("ANODIZER_GITHUB_TOKEN"))
            .or_else(|| env.var("GITHUB_TOKEN"))
    };

    let (mut closed, mut already_closed, mut failed) = (0usize, 0usize, 0usize);
    for t in &targets {
        let label = format!("{}/{}", t.upstream_owner, t.upstream_repo);
        let env_hint = t.token_env_var.as_deref().unwrap_or(DEFAULT_TOKEN_ENV_VAR);

        let Some(token) = resolve_token(t) else {
            log.warn(&format!(
                "skipped rollback for head {}:{} — no schemastore token resolvable for \
                 {label} (env var ${env_hint} / ANODIZER_GITHUB_TOKEN / GITHUB_TOKEN \
                 all unset)",
                t.fork_owner, t.branch
            ));
            continue;
        };

        let pr_numbers = match crate::util::find_open_pr_numbers_for_head(
            &t.upstream_owner,
            &t.upstream_repo,
            &t.fork_owner,
            &t.branch,
            Some(&token),
            env_hint,
        ) {
            Ok(v) => v,
            Err(e) => {
                log.warn(&format!(
                    "failed to query schemastore upstream {label} for open PRs (head {}:{}): \
                     {e}; manual cleanup required",
                    t.fork_owner, t.branch
                ));
                continue;
            }
        };

        // An empty open-PR set means either the PR was already closed/deleted,
        // OR it was MERGED — a merged PR is no longer `state=open`, and a close
        // cannot undo a merge. Either way there is nothing to PATCH; tell the
        // operator a manual revert PR is needed in the merged case.
        if pr_numbers.is_empty() {
            log.warn(&format!(
                "no open schemastore PR found for head {}:{} against {label}; if it was \
                 merged, closing cannot undo it — open a manual revert PR; otherwise verify \
                 it was already closed",
                t.fork_owner, t.branch
            ));
            continue;
        }

        for n in pr_numbers {
            let pr_url = format!("https://github.com/{label}/pull/{n}");
            log.status(&format!("closing schemastore PR {label} ({pr_url})"));
            match crate::util::close_pr_via_api(&t.upstream_owner, &t.upstream_repo, n, &token) {
                crate::util::CloseOutcome::Closed => closed += 1,
                crate::util::CloseOutcome::AlreadyClosed => {
                    already_closed += 1;
                    log.status(&format!(
                        "schemastore PR {label} ({pr_url}) already closed/deleted upstream — \
                         rollback noticed the existing state"
                    ));
                }
                crate::util::CloseOutcome::Failed(err) => {
                    failed += 1;
                    log.warn(&crate::publisher_helpers::rollback_failure_warning_msg(
                        "schemastore",
                        &label,
                        &pr_url,
                        &err,
                        Some(env_hint),
                    ));
                }
            }
        }
    }

    log.status(&format!(
        "schemastore rollback closed {closed}, already-closed {already_closed}, failed {failed}"
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::publish_evidence::{KrewExtra, KrewTargetSnapshot, SchemastoreExtra};

    fn target(branch: &str) -> SchemastoreTargetSnapshot {
        SchemastoreTargetSnapshot {
            upstream_owner: "SchemaStore".to_string(),
            upstream_repo: "schemastore".to_string(),
            fork_owner: "acme".to_string(),
            branch: branch.to_string(),
            token_env_var: Some(DEFAULT_TOKEN_ENV_VAR.to_string()),
        }
    }

    #[test]
    fn decode_extracts_schemastore_targets() {
        let extra = PublishEvidenceExtra::Schemastore(SchemastoreExtra {
            schemastore_targets: vec![target("schemastore-v1.0.0")],
        });
        let got = decode_schemastore_targets(&extra);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].branch, "schemastore-v1.0.0");
    }

    #[test]
    fn decode_returns_empty_for_other_variants() {
        assert!(decode_schemastore_targets(&PublishEvidenceExtra::Empty).is_empty());
        let krew = PublishEvidenceExtra::Krew(KrewExtra {
            krew_targets: vec![KrewTargetSnapshot {
                target: "foo".to_string(),
                upstream_owner: "kubernetes-sigs".to_string(),
                upstream_repo: "krew-index".to_string(),
                fork_owner: "acme".to_string(),
                branch: "foo-v1.0.0".to_string(),
                token_env_var: None,
            }],
        });
        assert!(decode_schemastore_targets(&krew).is_empty());
    }

    #[test]
    fn dedup_collapses_identical_heads() {
        let dup = vec![
            target("schemastore-v1.0.0"),
            target("schemastore-v1.0.0"),
            target("schemastore-v2.0.0"),
        ];
        let out = dedup_targets(dup);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].branch, "schemastore-v1.0.0");
        assert_eq!(out[1].branch, "schemastore-v2.0.0");
    }
}
