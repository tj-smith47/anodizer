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

        let pr_numbers = match crate::util::find_open_pr_numbers_for_head_with_env(
            &t.upstream_owner,
            &t.upstream_repo,
            &t.fork_owner,
            &t.branch,
            Some(&token),
            env_hint,
            env,
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
            match crate::util::close_pr_via_api_with_env(
                &t.upstream_owner,
                &t.upstream_repo,
                n,
                &token,
                env,
            ) {
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

    // -------------------------------------------------------------------------
    // Rollback HTTP path — driven through the scripted responder.
    //
    // The find-open-PR (GET …/pulls) and close-PR (PATCH …/pulls/<n>) calls
    // resolve their GitHub API base through the injected env source's
    // `ANODIZER_GITHUB_API_BASE` override, so these tests redirect the whole
    // flow at an in-process responder WITHOUT mutating the process env — no
    // `#[serial]` / env_mutex needed (the seam is per-`Context`, not global).
    // Credentials come from the same injected `MapEnvSource`.
    // -------------------------------------------------------------------------

    use anodizer_core::log::LogCapture;
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::test_helpers::scripted_responder::{
        ScriptedRoute, spawn_scripted_responder,
    };

    /// Build a single-target evidence whose head is `acme:<branch>` against
    /// `SchemaStore/schemastore`, the coordinates the wire routes key on.
    fn evidence_with_target(branch: &str) -> PublishEvidence {
        let mut ev = PublishEvidence::new("schemastore");
        ev.extra = PublishEvidenceExtra::Schemastore(SchemastoreExtra {
            schemastore_targets: vec![target(branch)],
        });
        ev
    }

    /// A `Context` whose env source carries the API-base override (pointing at
    /// `addr`) plus a resolvable `SCHEMASTORE_TOKEN`, and a log capture so the
    /// per-target summary/warn lines can be asserted.
    fn ctx_pointing_at(
        addr: std::net::SocketAddr,
        capture: &LogCapture,
    ) -> anodizer_core::context::Context {
        let mut ctx = TestContextBuilder::new()
            .env("ANODIZER_GITHUB_API_BASE", format!("http://{addr}"))
            .env("SCHEMASTORE_TOKEN", "sekret-token")
            .build();
        ctx.with_log_capture(capture.clone());
        ctx
    }

    /// Happy path: a single open PR is found for the head, then closed via a
    /// `PATCH …/pulls/<n>` carrying `{"state":"closed"}` and bearer auth. The
    /// summary reports exactly one close.
    #[test]
    fn rollback_finds_then_closes_open_pr() {
        let capture = LogCapture::new();
        let (addr, log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "GET",
                path_pattern: "/repos/SchemaStore/schemastore/pulls?state=open&head=acme:schemastore-v1.0.0&per_page=100",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 14\r\n\r\n[{\"number\":7}]",
                times: None,
            },
            ScriptedRoute {
                method: "PATCH",
                path_pattern: "/repos/SchemaStore/schemastore/pulls/7",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
                times: None,
            },
        ]);
        let mut ctx = ctx_pointing_at(addr, &capture);

        rollback_publish(&mut ctx, &evidence_with_target("schemastore-v1.0.0"))
            .expect("rollback returns Ok even on success");

        let entries = log.lock().unwrap();
        assert_eq!(entries.len(), 2, "one list GET then one close PATCH");

        let list = &entries[0];
        assert_eq!(list.method, "GET");
        assert!(
            list.path.contains("head=acme:schemastore-v1.0.0"),
            "{}",
            list.path
        );
        assert_eq!(list.header("authorization"), Some("Bearer sekret-token"));

        let close = &entries[1];
        assert_eq!(close.method, "PATCH");
        assert_eq!(close.path, "/repos/SchemaStore/schemastore/pulls/7");
        assert_eq!(close.header("authorization"), Some("Bearer sekret-token"));
        assert!(
            close.body.contains("\"state\":\"closed\""),
            "close payload must set state=closed: {}",
            close.body
        );

        let all = capture.all_messages();
        assert!(
            all.iter().any(|(_, m)| m.contains("closed 1")
                && m.contains("already-closed 0")
                && m.contains("failed 0")),
            "summary must report exactly one close; got: {all:?}"
        );
    }

    /// An empty open-PR list means the PR was already closed OR merged. A
    /// merged PR cannot be undone by a close, so the operator must be told a
    /// manual revert is needed — and NO PATCH is fired.
    #[test]
    fn rollback_warns_no_open_pr_and_skips_close() {
        let capture = LogCapture::new();
        let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/SchemaStore/schemastore/pulls?state=open&head=acme:schemastore-v2.0.0&per_page=100",
            response: "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n[]",
            times: None,
        }]);
        let mut ctx = ctx_pointing_at(addr, &capture);

        rollback_publish(&mut ctx, &evidence_with_target("schemastore-v2.0.0"))
            .expect("rollback Ok");

        let entries = log.lock().unwrap();
        assert_eq!(
            entries.len(),
            1,
            "only the list GET fires — no PATCH on an empty set"
        );

        let warns = capture.warn_messages();
        assert!(
            warns.iter().any(|m| m.contains("no open schemastore PR")
                && m.contains("merged")
                && m.contains("manual revert PR")),
            "merged/closed PR must warn about a manual revert; got: {warns:?}"
        );
    }

    /// A non-2xx on the list query (here a 500) is a per-target failure: it
    /// warns with the upstream label and `continue`s, and the function still
    /// returns Ok so a sibling publisher's rollback is not masked.
    #[test]
    fn rollback_warns_on_list_query_error_and_returns_ok() {
        let capture = LogCapture::new();
        let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/SchemaStore/schemastore/pulls?state=open&head=acme:schemastore-v3.0.0&per_page=100",
            response: "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 5\r\n\r\noops!",
            times: None,
        }]);
        let mut ctx = ctx_pointing_at(addr, &capture);

        rollback_publish(&mut ctx, &evidence_with_target("schemastore-v3.0.0"))
            .expect("rollback must not bubble Err on a query failure");

        let warns = capture.warn_messages();
        assert!(
            warns
                .iter()
                .any(|m| m.contains("failed to query schemastore upstream")
                    && m.contains("SchemaStore/schemastore")),
            "a list-query failure must warn naming the upstream; got: {warns:?}"
        );
    }

    /// A `404` on the close PATCH is the "desired end-state already true"
    /// signal (the PR is gone): it buckets as already-closed, not failed.
    #[test]
    fn rollback_close_404_buckets_as_already_closed() {
        let capture = LogCapture::new();
        let (addr, _log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "GET",
                path_pattern: "/repos/SchemaStore/schemastore/pulls?state=open&head=acme:schemastore-v4.0.0&per_page=100",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 15\r\n\r\n[{\"number\":42}]",
                times: None,
            },
            ScriptedRoute {
                method: "PATCH",
                path_pattern: "/repos/SchemaStore/schemastore/pulls/42",
                response: "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
                times: None,
            },
        ]);
        let mut ctx = ctx_pointing_at(addr, &capture);

        rollback_publish(&mut ctx, &evidence_with_target("schemastore-v4.0.0"))
            .expect("rollback Ok");

        let all = capture.all_messages();
        assert!(
            all.iter().any(|(_, m)| m.contains("closed 0")
                && m.contains("already-closed 1")
                && m.contains("failed 0")),
            "a 404 close must count as already-closed; got: {all:?}"
        );
    }

    /// A `500` on the close PATCH is a genuine failure: it buckets as failed
    /// (with a per-failure warn) but the function still returns Ok.
    #[test]
    fn rollback_close_500_buckets_as_failed_but_returns_ok() {
        let capture = LogCapture::new();
        let (addr, _log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "GET",
                path_pattern: "/repos/SchemaStore/schemastore/pulls?state=open&head=acme:schemastore-v5.0.0&per_page=100",
                response: "HTTP/1.1 200 OK\r\nContent-Length: 14\r\n\r\n[{\"number\":9}]",
                times: None,
            },
            ScriptedRoute {
                method: "PATCH",
                path_pattern: "/repos/SchemaStore/schemastore/pulls/9",
                response: "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 6\r\n\r\nboom!!",
                times: None,
            },
        ]);
        let mut ctx = ctx_pointing_at(addr, &capture);

        rollback_publish(&mut ctx, &evidence_with_target("schemastore-v5.0.0"))
            .expect("rollback Ok despite a close failure");

        let all = capture.all_messages();
        assert!(
            all.iter().any(|(_, m)| m.contains("closed 0")
                && m.contains("already-closed 0")
                && m.contains("failed 1")),
            "a 500 close must count as failed; got: {all:?}"
        );
        let warns = capture.warn_messages();
        assert!(
            warns.iter().any(|m| m.contains("schemastore")),
            "a close failure must surface a per-failure warn; got: {warns:?}"
        );
    }

    /// With a recorded target but NO token resolvable from the injected env,
    /// the per-target loop warns (naming the env var) and skips WITHOUT firing
    /// any network call — guarding against a credential-less GitHub request.
    #[test]
    fn rollback_skips_target_when_no_token_resolvable() {
        let capture = LogCapture::new();
        // Bind then drop a listener to obtain an address that refuses
        // connections — proving the skip arm makes no request (a connect would
        // surface as a query-failure warn instead of the no-token warn).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let dead_addr = listener.local_addr().expect("addr");
        drop(listener);

        // env carries the API base but NONE of SCHEMASTORE_TOKEN /
        // ANODIZER_GITHUB_TOKEN / GITHUB_TOKEN.
        let mut ctx = TestContextBuilder::new()
            .env("ANODIZER_GITHUB_API_BASE", format!("http://{dead_addr}"))
            .env("UNRELATED", "x")
            .build();
        ctx.with_log_capture(capture.clone());

        rollback_publish(&mut ctx, &evidence_with_target("schemastore-v6.0.0"))
            .expect("rollback Ok");

        let warns = capture.warn_messages();
        assert!(
            warns
                .iter()
                .any(|m| m.contains("no schemastore token resolvable")
                    && m.contains("SCHEMASTORE_TOKEN")),
            "no-token skip must warn naming the env var; got: {warns:?}"
        );
        let all = capture.all_messages();
        assert!(
            all.iter().any(|(_, m)| m.contains("closed 0")
                && m.contains("already-closed 0")
                && m.contains("failed 0")),
            "no-token skip must leave every counter at zero; got: {all:?}"
        );
    }
}
