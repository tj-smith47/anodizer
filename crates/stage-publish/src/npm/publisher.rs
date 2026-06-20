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

/// The GitHub Actions OIDC request pair, as an all-of preflight requirement.
/// Both vars are injected by GitHub only when the workflow grants
/// `id-token: write`; the npm publish exchanges them for a registry id-token.
fn oidc_requirement() -> anodizer_core::EnvRequirement {
    anodizer_core::EnvRequirement::EnvAllOf { vars: oidc_vars() }
}

/// The two GitHub Actions OIDC request vars as an owned `Vec`. Single source of
/// truth shared by [`oidc_requirement`] and the `Auto`-mode any-of gate.
fn oidc_vars() -> Vec<String> {
    super::publish::OIDC_ENV_VARS
        .iter()
        .map(|s| s.to_string())
        .collect()
}

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

    fn retain_on_rollback(&self) -> bool {
        Self::resolved_retain_on_rollback(self)
    }

    /// Preflight credentials per active `npms[]` entry, gated on each entry's
    /// [`NpmAuthMode`](anodizer_core::config::NpmAuthMode) (the same field
    /// `resolve_auth_for_package` reads at publish time):
    ///
    /// * `Token` — the token is mandatory: a templated `cfg.token`'s env refs,
    ///   else the `NPM_TOKEN` fallback.
    /// * `Oidc` — strictly the GitHub Actions OIDC request pair
    ///   (`ACTIONS_ID_TOKEN_REQUEST_URL` + `_TOKEN`); `NPM_TOKEN` is *not*
    ///   required, mirroring the resolver's refusal to fall back to a token.
    /// * `Auto` — satisfied by **either** a token **or** an OIDC context, so
    ///   preflight only fails the genuinely-credential-less case.
    ///
    /// The npm CLI is always required.
    fn requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        use anodizer_core::config::NpmAuthMode;
        let active: Vec<_> = ctx
            .config
            .npms
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
            .collect();
        if active.is_empty() {
            return Vec::new();
        }
        let mut out = vec![anodizer_core::EnvRequirement::Tool {
            name: "npm".to_string(),
        }];
        for entry in active {
            let token_req = crate::publisher_helpers::secret_requirement(
                entry.token.as_deref(),
                crate::npm::manifest::token_env_var(entry),
            );
            match entry.auth {
                // Token-only: the token is mandatory, exactly as before.
                NpmAuthMode::Token => out.extend(token_req),
                // Strict OIDC: the run path errors if the Actions request pair
                // is absent and never falls back to a token, so NPM_TOKEN is
                // deliberately NOT required here.
                NpmAuthMode::Oidc => out.push(oidc_requirement()),
                // Auto resolves per-package at publish time (existing package +
                // OIDC context → OIDC; brand-new package → token). Preflight
                // can only apply a COARSE token-OR-OIDC gate: it catches the
                // zero-credential (anonymous) case without false-failing the
                // valid OIDC-only existing-package path. The precise decision —
                // including the brand-new-package-needs-token error — stays in
                // `resolve_auth_for_package`, the runtime authority.
                NpmAuthMode::Auto => match token_req {
                    // Literal `cfg.token` → the credential is always inline.
                    None => {}
                    Some(anodizer_core::EnvRequirement::EnvAllOf { vars }) => {
                        let mut any = vars;
                        any.extend(oidc_vars());
                        out.push(anodizer_core::EnvRequirement::EnvAnyOf { vars: any });
                    }
                    Some(_) => unreachable!("secret_requirement yields EnvAllOf or None"),
                },
            }
        }
        out
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        let log = ctx.logger("publish");
        let entries = ctx.config.npms.clone().unwrap_or_default();
        if entries.is_empty() {
            log.status("no `npms:` entries configured");
            return Ok(anodizer_core::PublishEvidence::new("npm"));
        }
        log.status(&format!(
            "starting npm publish for {} entry(ies)",
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
        for cfg in entries.iter() {
            // Per-crate associations are out of scope for the top-level
            // `npms:` block — the first crate name (or the project name) is
            // the package-name fallback for an unnamed entry.
            let crate_name = ctx
                .config
                .crates
                .first()
                .map(|c| c.name.clone())
                .unwrap_or_else(|| ctx.config.project_name.clone());
            // Name the entry by what the operator recognises — the npm
            // package name, its `id`, or the resolved crate name — never the
            // raw config index, which is meaningless outside the YAML file.
            let label = cfg
                .name
                .clone()
                .filter(|n| !n.is_empty())
                .or_else(|| cfg.id.clone().filter(|i| !i.is_empty()))
                .unwrap_or_else(|| crate_name.clone());
            log.status(&format!("processing npm package '{}'", label));
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
                    "npm rollback of '{}@{}' skipped — env var ${} is unset; \
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
                        "npm rollback of '{}@{}' could not create .npmrc temp dir ({:#}); \
                         manual cleanup required",
                        t.package, t.version, e
                    ));
                    failed += 1;
                    continue;
                }
            };
            // Rollback (`npm unpublish`) requires a long-lived token — OIDC
            // mints short-lived publish-only credentials that cannot unpublish.
            // The empty-token skip above already routes OIDC-published packages
            // to the manual-unpublish warning.
            let auth = super::publish::NpmAuth::Token(token);
            if let Err(e) = super::publish::write_npmrc(cfg_dir.path(), &t.registry, &auth, None) {
                log.warn(&format!(
                    "npm rollback of '{}@{}' could not write .npmrc ({:#}); \
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
                    log.status(&format!("unpublished '{}@{}'", t.package, t.version));
                    succeeded += 1;
                }
                Err(e) => {
                    log.warn(&format!(
                        "failed to unpublish '{}@{}' ({:#}); \
                         after 72h npm no longer permits unpublish — manual \
                         deprecation may be the only remediation",
                        t.package, t.version, e
                    ));
                    failed += 1;
                }
            }
        }
        log.status(&format!(
            "npm rollback complete — {} unpublished, {} failure(s)",
            succeeded, failed
        ));
        Ok(())
    }

    fn preflight(&self, _ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        Ok(anodizer_core::PreflightCheck::Pass)
    }
}
