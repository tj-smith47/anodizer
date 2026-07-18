use super::*;

// ---------------------------------------------------------------------------
// CloudsmithPublisher (Publisher trait wrapper)
// ---------------------------------------------------------------------------

// Wraps [`publish_to_cloudsmith`] in the [`anodizer_core::Publisher`] trait
// so the dispatch path (see [`crate::registry::configured_publishers`])
// can drive Cloudsmith uploads alongside every other publisher.
//
// Group: [`anodizer_core::PublisherGroup::Assets`] (uploadable packages,
// server-side deletable). `required = false`.
//
// Rollback shape: per uploaded package, issue
// `DELETE /v1/packages/<org>/<repo>/<slug>/` with the same `CLOUDSMITH_*`
// token used for the upload. The slug is the per-package permanent
// identifier returned by the step-3 `packages/upload/<format>/` response
// and is captured into `CloudsmithTarget.slug` so [`PublishEvidence::extra`]
// (`cloudsmith_targets` key) carries it across the publish/rollback split.
// Targets whose slug couldn't be parsed (older evidence written before
// B13, or a response-shape change) degrade to the warn-only manual-cleanup
// checklist via [`cloudsmith_manual_cleanup_msg`]. Per-target DELETE
// failures (non-2xx, transport errors) emit a warn and continue —
// rollback is best-effort and a single 5xx must not orphan the remaining
// packages.
simple_publisher!(
    CloudsmithPublisher,
    "cloudsmith",
    anodizer_core::PublisherGroup::Assets,
    false,
    Some("CLOUDSMITH_API_KEY package_delete"),
);

/// One Cloudsmith upload target as recorded in evidence. Operator-readable
/// `(org, repo, filename)` tuples drive the rollback warn line; the optional
/// `slug` (Cloudsmith's per-package permanent identifier, returned by the
/// step-3 `packages/upload/<format>/` response) lets [`rollback`] issue a
/// real `DELETE /v1/packages/<org>/<repo>/<slug>/` instead of a warn-only
/// manual-cleanup checklist.
///
/// Aliased to the core-owned snapshot so the evidence schema lives in
/// [`anodizer_core::publish_evidence`] and credential-shaped fields
/// have no slot to land in. `slug` stays `Option` because evidence
/// emitted before slug-capture didn't carry it; rollback falls back
/// to the warn-only path (see [`cloudsmith_manual_cleanup_msg`]) for
/// any target whose slug is absent.
pub(crate) type CloudsmithTarget = anodizer_core::publish_evidence::CloudsmithTargetSnapshot;

/// Encode the per-target tuples into the typed
/// [`PublishEvidenceExtra::Cloudsmith`] variant.
pub(crate) fn encode_cloudsmith_targets(
    targets: &[CloudsmithTarget],
) -> anodizer_core::PublishEvidenceExtra {
    anodizer_core::PublishEvidenceExtra::Cloudsmith(
        anodizer_core::publish_evidence::CloudsmithExtra {
            cloudsmith_targets: targets.to_vec(),
        },
    )
}

/// Decode the typed Cloudsmith variant back into structured targets.
/// Returns an empty vec when the variant doesn't match — the rollback
/// then surfaces the empty-evidence warn instead of crashing.
pub(crate) fn decode_cloudsmith_targets(
    extra: &anodizer_core::PublishEvidenceExtra,
) -> Vec<CloudsmithTarget> {
    match extra {
        anodizer_core::PublishEvidenceExtra::Cloudsmith(c) => c.cloudsmith_targets.clone(),
        _ => Vec::new(),
    }
}

/// The per-target warn line a rollback emits as a FALLBACK when no slug is
/// available in evidence (legacy evidence written before B13 added slug
/// capture, or a step-3 `packages/upload/<format>/` response that didn't
/// surface a slug). Operator-readable; renders the load-bearing
/// `<org>/<repo>` location plus the filename to remove. Exposed as a
/// helper so tests can pin the wording without intercepting stderr.
///
/// The PRIMARY rollback path issues a real
/// `DELETE /v1/packages/<org>/<repo>/<slug>/` against the Cloudsmith API
/// (see [`<CloudsmithPublisher as anodizer_core::Publisher>::rollback`]);
/// this helper is reached only when `target.slug` is `None`.
pub(crate) fn cloudsmith_manual_cleanup_msg(target: &CloudsmithTarget) -> String {
    format!(
        "manually withdraw '{}' from cloudsmith {}/{} (per-package slug not surfaced in evidence; delete via the Cloudsmith dashboard)",
        target.filename, target.org, target.repo
    )
}

/// Top-level `cloudsmiths:` entries whose `skip:`/`if:` evaluates active
/// right now. Shared by [`anodizer_core::Publisher::requirements`] and
/// [`anodizer_core::Publisher::config_fully_inactive`] so the two cannot
/// diverge. `preflight` keeps its own loop (it needs per-entry endpoint
/// resolution alongside the filter, not just a boolean).
pub(crate) fn active_cloudsmith_configs(
    ctx: &Context,
) -> Vec<&anodizer_core::config::CloudSmithConfig> {
    ctx.config
        .cloudsmiths
        .iter()
        .flatten()
        .filter(|entry| {
            !crate::publisher_helpers::entry_inactive(
                ctx,
                entry.skip.as_ref(),
                None,
                entry.if_condition.as_deref(),
            )
        })
        .collect()
}

impl anodizer_core::Publisher for CloudsmithPublisher {
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

    fn config_fully_inactive(&self, ctx: &Context) -> bool {
        active_cloudsmith_configs(ctx).is_empty()
    }

    fn requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        // Same env-var-name resolution the upload path uses: a (templated)
        // `secret_name` per entry, defaulting to CLOUDSMITH_TOKEN.
        active_cloudsmith_configs(ctx)
            .into_iter()
            .map(|entry| {
                let var = crate::util::resolve_secret_name(
                    ctx,
                    entry.secret_name.as_deref(),
                    "CLOUDSMITH_TOKEN",
                );
                anodizer_core::EnvRequirement::EnvAllOf { vars: vec![var] }
            })
            .collect()
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        let log = ctx.logger("publish");
        // The upload path returns the live target list (with slugs
        // populated when step 3's response carried one) so evidence
        // records what we actually uploaded — not a post-hoc walk of
        // config + artifacts, which can drift from the upload list and
        // never captures the slug. SkipIdempotent matches (artifact
        // already on Cloudsmith with matching md5) are NOT in `targets`
        // because rollback only undoes what THIS run did.
        let targets = publish_to_cloudsmith(ctx, &log)?;
        let mut evidence = anodizer_core::PublishEvidence::new("cloudsmith");
        // The `artifact_paths` slot keeps the operator-readable
        // `<org>/<repo>/<filename>` form for the text-only
        // --rollback-only summary; the structured copy in `extra` is the
        // authoritative source for the DELETE call.
        let path_view: Vec<std::path::PathBuf> = targets
            .iter()
            .map(|t| std::path::PathBuf::from(format!("{}/{}/{}", t.org, t.repo, t.filename)))
            .collect();
        if let Some(first) = path_view.first() {
            evidence.primary_ref = Some(first.display().to_string());
        }
        evidence.artifact_paths = path_view;
        evidence.extra = encode_cloudsmith_targets(&targets);
        Ok(evidence)
    }

    fn rollback(
        &self,
        ctx: &mut Context,
        evidence: &anodizer_core::PublishEvidence,
    ) -> anyhow::Result<()> {
        let log = ctx.logger("publish");
        let targets = decode_cloudsmith_targets(&evidence.extra);
        if targets.is_empty() {
            log.warn(&crate::publisher_helpers::rollback_empty_warning_msg(
                "cloudsmith",
                "upload targets",
            ));
            return Ok(());
        }

        // Resolve the API token once; if it's absent we cannot DELETE
        // anything, so fall back to the warn-only manual-cleanup
        // checklist for every target. `CLOUDSMITH_API_KEY` is the
        // rollback-scope env name declared by `rollback_scope_needed`.
        let token = ctx.env_var("CLOUDSMITH_API_KEY");
        if token.is_none() {
            log.warn(
                "CLOUDSMITH_API_KEY not set; emitting manual-cleanup checklist instead of DELETE",
            );
        }

        let client = anodizer_core::http::blocking_client(std::time::Duration::from_secs(30))
            .context("cloudsmith: failed to build HTTP client for rollback")?;
        let policy = ctx.retry_policy();
        let deadline = ctx.retry_deadline();
        let env = ctx.env_source_arc();

        let mut deleted = 0usize;
        let mut already_absent = 0usize;
        let mut failed = 0usize;
        let mut warn_only = 0usize;

        for target in &targets {
            // Two ways into the warn-only fallback:
            //   1. No token at all (handled above; warn already emitted).
            //   2. No slug for this target (older evidence, or step-3
            //      response shape change).
            let Some(slug) = target.slug.as_deref() else {
                log.warn(&cloudsmith_manual_cleanup_msg(target));
                warn_only += 1;
                continue;
            };
            let Some(tok) = token.as_deref() else {
                log.warn(&cloudsmith_manual_cleanup_msg(target));
                warn_only += 1;
                continue;
            };

            let url = format!(
                "{}/packages/{}/{}/{}/",
                cloudsmith_api_base_from(env.as_ref()),
                target.org,
                target.repo,
                slug
            );
            log.verbose(&format!("DELETE {}", url));
            let label = "packages/delete";
            match retry_request(label, &target.filename, &policy, deadline, &log, || {
                client
                    .delete(&url)
                    .header("Authorization", format!("token {}", tok))
                    .header("Accept", "application/json")
                    .send()
            }) {
                Ok((status, _body)) => {
                    if status.is_success() {
                        deleted += 1;
                    } else {
                        // `retry_http_blocking` Strict mode treats only
                        // 2xx as success, so 4xx (other than 404/410) and
                        // 5xx already raise an `Err` here. This arm is
                        // unreachable, but guard it defensively.
                        failed += 1;
                        log.warn(&format!(
                            "DELETE {} returned HTTP {} (manual cleanup may be required)",
                            url, status
                        ));
                    }
                }
                Err(err) => {
                    // 404 / 410 = package already absent (operator deleted
                    // via the dashboard, or a prior partial rollback ran).
                    // Detect by substring on the shaped error message.
                    let msg = format!("{err:#}");
                    if msg.contains("HTTP 404") || msg.contains("HTTP 410") {
                        already_absent += 1;
                        log.status(&format!("DELETE {} already absent (404/410)", url));
                    } else {
                        failed += 1;
                        log.warn(&format!(
                            "DELETE {} failed ({}); manual cleanup may be required",
                            url, err
                        ));
                    }
                }
            }
        }

        log.status(&format!(
            "cloudsmith rollback complete — {} deleted, {} already absent, {} failed, {} warn-only (slug/token unavailable)",
            deleted, already_absent, failed, warn_only
        ));
        Ok(())
    }

    /// Live pre-publish gate. For every active `cloudsmiths[]` entry whose
    /// upload token resolves, probe `GET {api_base}/packages/{org}/{repo}/` with
    /// `Authorization: token <key>` — the same coordinates + auth header the
    /// upload path uses. A rejected token (401/403) or a missing repo (404)
    /// proves the publish cannot proceed; an unreachable host is the failure a
    /// no-op preflight let slip past the one-way doors. Severity follows
    /// [`required()`](anodizer_core::Publisher::required) so the gate is never
    /// stricter than the publish it precedes.
    ///
    /// Entries with no resolvable token are left to `requirements()` (which
    /// gates token *presence*); this probe only proves a *present* token is
    /// actually accepted.
    fn preflight(&self, ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        use crate::publisher_preflight::{
            FailSeverity, ProbeAuth, ProbeMethod, default_probe_client, merge, probe_http_endpoint,
        };
        use anodizer_core::PreflightCheck;

        let policy = anodizer_core::retry::RetryPolicy::PREFLIGHT;
        let fail = FailSeverity::for_required(Self::resolved_required(self));
        let api_base = cloudsmith_api_base_from(ctx.env_source());

        let mut acc = PreflightCheck::Pass;
        for entry in ctx.config.cloudsmiths.iter().flatten() {
            if crate::publisher_helpers::entry_inactive(
                ctx,
                entry.skip.as_ref(),
                None,
                entry.if_condition.as_deref(),
            ) {
                continue;
            }
            // An absent org/repo is config-validation territory the upload path
            // already fails loud on; don't manufacture a duplicate blocker here.
            let (Some(org_raw), Some(repo_raw)) =
                (entry.organization.as_deref(), entry.repository.as_deref())
            else {
                continue;
            };
            let org = ctx
                .render_template(org_raw)
                .unwrap_or_else(|_| org_raw.to_string());
            let repo = ctx
                .render_template(repo_raw)
                .unwrap_or_else(|_| repo_raw.to_string());
            if org.trim().is_empty() || repo.trim().is_empty() {
                continue;
            }
            let token_env = crate::util::resolve_secret_name(
                ctx,
                entry.secret_name.as_deref(),
                "CLOUDSMITH_TOKEN",
            );
            let Some(token) = ctx.env_var(&token_env).filter(|t| !t.is_empty()) else {
                continue;
            };
            let client = match default_probe_client() {
                Ok(c) => c,
                Err(e) => {
                    acc = merge(
                        acc,
                        PreflightCheck::Warning(format!(
                            "cloudsmith: could not build HTTP client for preflight ({e})"
                        )),
                    );
                    continue;
                }
            };
            // `?page_size=1` keeps the authed list cheap; the repo coordinate is
            // the auth + existence surface (401/403 = bad token, 404 = bad repo).
            let url = format!("{api_base}/packages/{org}/{repo}/?page_size=1");
            acc = merge(
                acc,
                probe_http_endpoint(
                    &client,
                    ProbeMethod::Get,
                    &url,
                    &ProbeAuth::Token(token),
                    "preflight: cloudsmith",
                    fail,
                    ctx.preflight_is_strict(),
                    &policy,
                    &ctx.logger("preflight"),
                ),
            );
        }
        Ok(acc)
    }

    fn skips_on_nightly(&self) -> bool {
        // Cloudsmith supports versioned packages; nightly uploads do not
        // clobber stable content and are allowed.
        false
    }

    fn retain_on_rollback(&self) -> bool {
        Self::resolved_retain_on_rollback(self)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
