use super::*;

// ---------------------------------------------------------------------------
// ArtifactoryPublisher (Publisher trait wrapper)
// ---------------------------------------------------------------------------

// Wraps [`publish_to_artifactory`] in the [`anodizer_core::Publisher`] trait
// so the dispatch path (see [`crate::registry::configured_publishers`])
// can drive Artifactory uploads alongside every other publisher.
//
// Group: [`anodizer_core::PublisherGroup::Assets`] (uploadable bytes,
// server-side deletable). `required = false`.
//
// Rollback shape: per uploaded URL, issue an HTTP DELETE with the same
// credential cascade `publish_to_artifactory` uses (basic auth from
// `username` + `password` plus per-entry `ARTIFACTORY_<NAME>_SECRET`
// override; the legacy `ARTIFACTORY_TOKEN` bearer is a last-resort
// fallback when no entry name was threaded through evidence). DELETEs
// fan out under a fixed concurrency cap (4) so a v0.2.0-sized 143-artifact
// rollback finishes in minutes, not over an hour. 404 / 410 responses are
// classified `AlreadyAbsent` (not `Failed`) so a re-run after a partial
// rollback doesn't print phantom failures. The rollback function returns
// Ok regardless of per-target outcome — the summary line + per-failure
// warns carry the operator-facing diagnosis.
simple_publisher!(
    ArtifactoryPublisher,
    "artifactory",
    anodizer_core::PublisherGroup::Assets,
    false,
    Some("ARTIFACTORY_TOKEN delete"),
);

/// Top-level `artifactories:` entries whose `skip:`/`if:` evaluates active
/// right now. Shared by [`anodizer_core::Publisher::requirements`] and
/// [`anodizer_core::Publisher::config_fully_inactive`] so the two cannot
/// diverge. `preflight` keeps its own loop (it needs per-entry credential
/// resolution alongside the filter, not just a boolean).
fn active_artifactory_configs(ctx: &Context) -> Vec<&anodizer_core::config::ArtifactoryConfig> {
    ctx.config
        .artifactories
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

impl anodizer_core::Publisher for ArtifactoryPublisher {
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
        active_artifactory_configs(ctx).is_empty()
    }

    fn requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        // Mirrors `resolve_http_credentials` (anonymous_ok = false): per
        // entry, each of username/password comes from the templated config
        // value or the `ARTIFACTORY_<NAME>_{USERNAME,SECRET}` env pair.
        let mut out = Vec::new();
        for entry in active_artifactory_configs(ctx) {
            let name_upper = entry
                .name
                .as_deref()
                .unwrap_or("")
                .to_uppercase()
                .replace('-', "_");
            if let Some(req) = crate::publisher_helpers::secret_requirement(
                entry.username.as_deref(),
                &format!("ARTIFACTORY_{}_USERNAME", name_upper),
            ) {
                out.push(req);
            }
            if let Some(req) = crate::publisher_helpers::secret_requirement(
                entry.password.as_deref(),
                &format!("ARTIFACTORY_{}_SECRET", name_upper),
            ) {
                out.push(req);
            }
        }
        out
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        let log = ctx.logger("publish");
        let summary = publish_to_artifactory(ctx, &log)?;
        // Every matched artifact was already present at its target path (an
        // idempotent re-run): record a SKIP, not a fresh publish.
        if summary.is_fully_idempotent_skip() {
            ctx.record_publisher_outcome(anodizer_core::PublisherOutcome::Skipped(
                anodizer_core::SkipReason::AlreadyPublished,
            ));
        }
        let mut evidence = anodizer_core::PublishEvidence::new("artifactory");
        let targets = collect_artifactory_targets(ctx);
        if let Some(first) = targets.first() {
            evidence.primary_ref = Some(first.url.clone());
        }
        evidence.artifact_paths = targets
            .iter()
            .map(|t| std::path::PathBuf::from(&t.url))
            .collect();
        evidence.extra = encode_artifactory_targets(&targets);
        Ok(evidence)
    }

    fn rollback(
        &self,
        ctx: &mut Context,
        evidence: &anodizer_core::PublishEvidence,
    ) -> anyhow::Result<()> {
        let log = ctx.logger("publish");
        if evidence.artifact_paths.is_empty() && evidence.primary_ref.is_none() {
            log.warn(&crate::publisher_helpers::rollback_empty_warning_msg(
                "artifactory",
                "upload URLs",
            ));
            return Ok(());
        }
        // Decode the structured (entry, url) pairs from evidence.extra so
        // each DELETE can resolve credentials through the publish path's
        // own resolver (basic auth + per-entry env override). When the
        // field is missing (older evidence, or a config change between
        // publish and rollback) fall back to URL-only deletion against
        // the legacy bearer ladder so existing rollbacks don't silently
        // break.
        let structured = decode_artifactory_targets(&evidence.extra);
        let token_env = ctx
            .env_var("ARTIFACTORY_TOKEN")
            .or_else(|| ctx.env_var("ARTIFACTORY_SECRET"));
        let client = match anodizer_core::http::blocking_client(std::time::Duration::from_secs(30))
        {
            Ok(c) => c,
            Err(e) => {
                log.warn(&format!(
                    "artifactory rollback failed to build HTTP client: {}; manual cleanup required",
                    e
                ));
                return Ok(());
            }
        };

        // Build (url, auth) pairs honouring structured evidence first,
        // falling back to URL-only deletion against the bearer ladder for
        // legacy / pruned-config rollbacks.
        let by_url: std::collections::HashMap<String, String> = structured
            .iter()
            .map(|t| (t.url.clone(), t.entry.clone()))
            .collect();
        let jobs: Vec<RollbackJob> = evidence
            .artifact_paths
            .iter()
            .map(|p| {
                let url = p.display().to_string();
                let basic_auth = by_url
                    .get(&url)
                    .and_then(|entry| resolve_rollback_credentials(ctx, entry))
                    .filter(|(u, p)| !u.is_empty() && !p.is_empty());
                let bearer = if basic_auth.is_none() {
                    token_env.clone()
                } else {
                    None
                };
                RollbackJob {
                    url,
                    basic_auth,
                    bearer,
                }
            })
            .collect();

        let (deleted, already_absent, failed) = parallel_delete(&client, &jobs, &log);
        log.status(&format!(
            "artifactory rollback deleted {} artifact(s), {} already absent, {} failure(s)",
            deleted, already_absent, failed
        ));
        Ok(())
    }

    /// Live pre-publish gate. For every active `artifactories[]` entry, probe the
    /// target's `scheme://host[:port]` origin (path/`{{ .Version }}` stripped so
    /// the probe needs no resolved tag) through the same mTLS client + basic-auth
    /// credential cascade the upload uses. A rejected credential (401/403) or an
    /// unreachable host surfaces as a Warning (this publisher is OPTIONAL — a
    /// failed upload must not abort the release). A 404/405/redirect at the
    /// origin root still proves the host is reachable and is treated as a pass.
    /// Severity follows [`required()`](anodizer_core::Publisher::required).
    ///
    /// Credentials are resolved with `anonymous_ok = false` (matching `run()`);
    /// an unresolved pair is `requirements()`'s domain and is skipped here.
    fn preflight(&self, ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        use crate::publisher_preflight::{
            FailSeverity, ProbeAuth, ProbeMethod, classify_http_endpoint, merge,
            reachability_outcome,
        };
        use anodizer_core::PreflightCheck;

        let policy = anodizer_core::retry::RetryPolicy::PREFLIGHT;
        let fail = FailSeverity::for_required(Self::resolved_required(self));

        let mut acc = PreflightCheck::Pass;
        for entry in ctx.config.artifactories.iter().flatten() {
            if crate::publisher_helpers::entry_inactive(
                ctx,
                entry.skip.as_ref(),
                None,
                entry.if_condition.as_deref(),
            ) {
                continue;
            }
            let Some(name) = entry.name.as_deref().filter(|n| !n.is_empty()) else {
                continue;
            };
            let Some(target) = entry.target.as_deref().filter(|t| !t.is_empty()) else {
                continue;
            };
            let Some(origin_template) = crate::uploads::target_origin(target) else {
                continue;
            };
            let url = match ctx.render_template(&origin_template) {
                Ok(u) if !u.trim().is_empty() => u,
                _ => continue,
            };
            let (username, password) = match crate::http_upload::resolve_http_credentials(
                ctx,
                &crate::http_upload::CredentialResolveSpec {
                    publisher: "artifactory",
                    entry_name: name,
                    config_username: entry.username.as_deref(),
                    config_password: entry.password.as_deref(),
                    env_prefix: "ARTIFACTORY",
                    anonymous_ok: false,
                },
            ) {
                Ok(creds) => creds,
                Err(_) => continue,
            };
            let auth = if username.is_empty() && password.is_empty() {
                ProbeAuth::None
            } else {
                ProbeAuth::Basic { username, password }
            };
            let client = match build_reqwest_client(
                entry.client_x509_cert.as_deref(),
                entry.client_x509_key.as_deref(),
                entry.trusted_certificates.as_deref(),
            ) {
                Ok(c) => c,
                Err(e) => {
                    acc = merge(
                        acc,
                        PreflightCheck::Warning(format!(
                            "artifactory: entry '{name}' HTTP client build failed in preflight ({e})"
                        )),
                    );
                    continue;
                }
            };
            let status = classify_http_endpoint(
                &client,
                ProbeMethod::Get,
                &url,
                &auth,
                "preflight: artifactory",
                &policy,
                ctx.retry_deadline(),
                &ctx.logger("preflight"),
            );
            acc = merge(
                acc,
                reachability_outcome(
                    status,
                    &url,
                    "preflight: artifactory",
                    fail,
                    ctx.preflight_is_strict(),
                ),
            );
        }
        Ok(acc)
    }

    fn skips_on_nightly(&self) -> bool {
        // Artifact repositories support versioned paths; nightly re-uploads
        // do not clobber stable content and are allowed.
        false
    }

    fn retain_on_rollback(&self) -> bool {
        Self::resolved_retain_on_rollback(self)
    }
}
