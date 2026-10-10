use super::*;

// Bound for parallel DELETE fan-out during rollback is shared with the
// git-revert publishers via [`crate::util::ROLLBACK_PARALLELISM`].
// Re-imported below so the local references in `parallel_delete` stay
// terse.
use crate::util::ROLLBACK_PARALLELISM;

/// Aliased to the core-owned snapshot so the evidence schema lives in
/// [`anodizer_core::publish_evidence`] and credential-shaped fields
/// have no slot to fill. The rollback path resolves credentials
/// from env at call time via the existing `ARTIFACTORY_<NAME>_*`
/// ladder; nothing about that flow persists in evidence.
pub(crate) type ArtifactoryTarget = anodizer_core::publish_evidence::ArtifactoryTargetSnapshot;

/// Encode the per-target `(entry, url)` pairs into the typed
/// [`PublishEvidenceExtra::Artifactory`](anodizer_core::PublishEvidenceExtra::Artifactory)
/// variant. Mirrors the wire
/// shape `{ "artifactory_targets": [...] }` that shipped pre-typed.
pub(crate) fn encode_artifactory_targets(
    targets: &[ArtifactoryTarget],
) -> anodizer_core::PublishEvidenceExtra {
    anodizer_core::PublishEvidenceExtra::Artifactory(
        anodizer_core::publish_evidence::ArtifactoryExtra {
            artifactory_targets: targets.to_vec(),
        },
    )
}

/// Decode the typed Artifactory variant into structured targets.
/// Returns an empty vec when the variant doesn't match — rollback
/// then falls back to URL-only deletion against the legacy
/// `ARTIFACTORY_TOKEN` ladder.
pub(crate) fn decode_artifactory_targets(
    extra: &anodizer_core::PublishEvidenceExtra,
) -> Vec<ArtifactoryTarget> {
    match extra {
        anodizer_core::PublishEvidenceExtra::Artifactory(a) => a.artifactory_targets.clone(),
        _ => Vec::new(),
    }
}

/// Resolve `(username, password)` for an artifactory entry at rollback
/// time, mirroring the exact credential cascade `publish_to_artifactory`
/// uses (config → `ARTIFACTORY_<NAME>_USERNAME` / `ARTIFACTORY_<NAME>_SECRET`
/// env, with the per-entry override honoured). Returns `None` when the
/// entry is no longer present in config (e.g. the operator pruned the
/// YAML between publish and rollback) so the caller can decide between
/// best-effort token fallback and skipping.
pub(crate) fn resolve_rollback_credentials(
    ctx: &Context,
    entry_name: &str,
) -> Option<(String, String)> {
    let entries = ctx.config.artifactories.as_ref()?;
    let entry = entries
        .iter()
        .find(|e| e.name.as_deref() == Some(entry_name))?;
    crate::http_upload::resolve_http_credentials(
        ctx,
        &crate::http_upload::CredentialResolveSpec {
            publisher: "artifactory",
            entry_name,
            config_username: entry.username.as_deref(),
            config_password: entry.password.as_deref(),
            env_prefix: "ARTIFACTORY",
            // Rollback is best-effort; tolerate anonymous so a missing
            // credential surfaces as a 401 in the deletion summary rather
            // than bailing here.
            anonymous_ok: true,
        },
    )
    .ok()
}

/// Outcome of one DELETE attempt against a single artifactory URL.
/// Produced by [`classify_delete_status`] so the per-URL response
/// can be aggregated into the summary line.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DeleteOutcome {
    Deleted,
    AlreadyAbsent,
    Failed(String),
}

/// Classify a DELETE response's status code into the rollback summary
/// bucket. 2xx → `Deleted`, 404 / 410 → `AlreadyAbsent`, everything else
/// → `Failed`. Pure helper so the bucket boundary can be unit-tested
/// without firing an HTTP request.
pub(crate) fn classify_delete_status(status: reqwest::StatusCode) -> DeleteOutcome {
    if status.is_success() {
        DeleteOutcome::Deleted
    } else if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::GONE {
        DeleteOutcome::AlreadyAbsent
    } else {
        DeleteOutcome::Failed(format!("HTTP {}", status))
    }
}

/// One rollback DELETE job: target URL + the auth to send with it.
/// `basic_auth` carries (username, password) when the entry tag in
/// `PublishEvidence::extra` resolved to a configured basic-auth pair;
/// otherwise `bearer` falls back to `ARTIFACTORY_TOKEN` /
/// `ARTIFACTORY_SECRET`. Both `None` is acceptable — the DELETE will
/// surface a 401 in the failed bucket rather than silently 200ing.
#[derive(Clone, Debug)]
pub(crate) struct RollbackJob {
    pub(crate) url: String,
    pub(crate) basic_auth: Option<(String, String)>,
    pub(crate) bearer: Option<String>,
}

/// Fan out per-URL DELETE requests under [`ROLLBACK_PARALLELISM`], applying
/// the resolved auth per request. Each request's outcome is classified via
/// [`classify_delete_status`] so 404 / 410 end up in `already_absent` instead
/// of `failed`. Returns `(deleted, already_absent, failed)` counts.
pub(crate) fn parallel_delete(
    client: &reqwest::blocking::Client,
    jobs: &[RollbackJob],
    log: &StageLogger,
) -> (usize, usize, usize) {
    use std::sync::Mutex;
    let counts = Mutex::new((0usize, 0usize, 0usize));
    let retry_scope = anodizer_core::retry::current_scope();
    let chunks = jobs.chunks(ROLLBACK_PARALLELISM);
    for chunk in chunks {
        std::thread::scope(|s| {
            let mut handles = Vec::with_capacity(chunk.len());
            for job in chunk {
                let retry_scope = retry_scope.clone();
                let client = client.clone();
                let url = job.url.clone();
                let basic_auth = job.basic_auth.clone();
                let bearer = job.bearer.clone();
                let log = log.clone();
                let counts = &counts;
                handles.push(s.spawn(move || {
                    let _scope = anodizer_core::retry::RetryScope::inherit(retry_scope);
                    log.verbose(&format!("DELETE {}", url));
                    let mut req = client.delete(&url);
                    if let Some((ref u, ref p)) = basic_auth {
                        req = req.basic_auth(u, Some(p));
                    } else if let Some(ref tok) = bearer {
                        req = req.bearer_auth(tok);
                    }
                    match req.send() {
                        Ok(resp) => {
                            let status = resp.status();
                            match classify_delete_status(status) {
                                DeleteOutcome::Deleted => {
                                    let mut c = crate::util::lock_recover(counts, &log, "artifactory");
                                    c.0 += 1;
                                }
                                DeleteOutcome::AlreadyAbsent => {
                                    let mut c = crate::util::lock_recover(counts, &log, "artifactory");
                                    c.1 += 1;
                                    log.status(&format!(
                                        "DELETE {} returned HTTP {} (already absent)",
                                        url, status
                                    ));
                                }
                                DeleteOutcome::Failed(_) => {
                                    let mut c = crate::util::lock_recover(counts, &log, "artifactory");
                                    c.2 += 1;
                                    log.warn(&format!(
                                        "DELETE {} returned HTTP {} (manual cleanup may be required)",
                                        url, status
                                    ));
                                }
                            }
                        }
                        Err(e) => {
                            let mut c = crate::util::lock_recover(counts, &log, "artifactory");
                            c.2 += 1;
                            log.warn(&format!(
                                "DELETE {} transport error: {} (manual cleanup may be required)",
                                url, e
                            ));
                        }
                    }
                }));
            }
            for h in handles {
                crate::util::join_or_warn(h, log, "artifactory");
            }
        });
    }
    // `into_inner` consumes the Mutex; poison here means a worker
    // panicked. Counter state is still valid (3-tuple of usize) so
    // recover and emit the summary rather than abandon the operator.
    match counts.into_inner() {
        Ok(c) => c,
        Err(poisoned) => {
            log.warn("artifactory mutex poisoned by worker panic; reporting counters as-of poison");
            poisoned.into_inner()
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
