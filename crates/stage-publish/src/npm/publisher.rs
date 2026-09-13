//! `NpmPublisher` — Submitter-group `Publisher` impl wrapping
//! [`publish_to_npm`].
//!
//! Classification:
//! * **Group**: Submitter — a published npm version number is a burned slot:
//!   npm rejects re-publishing a version that has ever been used (a 24h lock
//!   survives even an `npm unpublish`, and unpublish itself is refused after
//!   72h or once a package has dependents). So a completed npm publish cannot be
//!   cleanly re-cut at the same version, which is exactly what the rollback
//!   guard must see — it counts toward `irreversibly_published` and refuses a
//!   same-version tag re-cut. (It is NOT Manager: Manager is
//!   server-side-deletable package-manager state — homebrew/scoop/nix — that
//!   a same-version re-cut can cleanly overwrite; npm cannot.)
//! * **Required default**: `true` — a failed npm publish matters for
//!   users who install via `npm i -g`; the operator should know the release is
//!   half-shipped.
//! * **Rollback scope**: `NPM_TOKEN unpublish` — the 72h-window `npm
//!   unpublish` is a best-effort rollback capability (like cargo's `yank`),
//!   not a reason to treat the slot as reclaimable for a re-cut.
//!
//! Evidence: one [`NpmTargetSnapshot`](anodizer_core::publish_evidence::NpmTargetSnapshot)
//! per published package (per-platform
//! packages + the metapackage in optional-deps mode). Skip / dry-run /
//! no-binaries paths produce no evidence.

use anodizer_core::context::Context;

use super::publish::publish_to_npm;

simple_publisher!(
    NpmPublisher,
    "npm",
    anodizer_core::PublisherGroup::Submitter,
    true,
    Some("NPM_TOKEN unpublish"),
);

/// Aliased to the core-owned snapshot so the evidence schema lives in
/// [`anodizer_core::publish_evidence`] and credential-shaped fields have no
/// slot to fill.
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
    super::auth::OIDC_ENV_VARS
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

/// Top-level `npms:` entries whose `skip:`/`if:` evaluates active right now.
/// `npms` is NOT crate-scoped, so this deliberately does not apply
/// `ctx.options.selected_crates` — mirrors `uploads.rs`'s
/// `active_upload_configs`. Single source of truth shared by
/// [`anodizer_core::Publisher::config_fully_inactive`], `requirements()`,
/// and `preflight()` so the active-entry definition can't diverge between
/// them.
fn active_npm_configs(ctx: &Context) -> Vec<&anodizer_core::config::NpmConfig> {
    ctx.config
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
        .collect()
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

    fn config_fully_inactive(&self, ctx: &Context) -> bool {
        active_npm_configs(ctx).is_empty()
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
        let active = active_npm_configs(ctx);
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
                    // `secret_requirement` only yields `EnvAllOf`/`None` today; if a
                    // future core change returns another shape, forward it verbatim
                    // (a slightly over-broad preflight gate) rather than panicking.
                    Some(other) => out.push(other),
                },
            }
        }
        out
    }

    /// Registry-presence reconcile: `Complete` only when EVERY package this
    /// run would publish (postinstall name, or optional-deps per-platform
    /// packages plus the metapackage unless `skip_metapackage`) already shows
    /// `GET {registry}/{pkg}/{version}` = 200. npm versions are immutable, so
    /// presence is completion per package; anything unenumerable (no built
    /// binaries yet, template/config errors) stays `Absent` so `run()` keeps
    /// surfacing the failure loudly, and a probe transport error is `Unknown`
    /// rather than a false "not published".
    fn reconcile(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::ReconcileState> {
        use anodizer_core::ReconcileState;
        let cfgs = active_npm_configs(ctx);
        if cfgs.is_empty() {
            return Ok(ReconcileState::Absent);
        }
        let policy = anodizer_core::retry::RetryPolicy::PREFLIGHT;
        let deadline = ctx.retry_deadline();
        let log = ctx.logger("publish");
        let crate_name = ctx
            .config
            .primary_crate_name()
            .map(str::to_string)
            .unwrap_or_else(|| ctx.config.project_name.clone());
        let version = ctx.version();
        let mut published = 0usize;
        for cfg in &cfgs {
            let Ok(registry) = crate::npm::manifest::resolve_registry(ctx, cfg) else {
                return Ok(ReconcileState::Absent);
            };
            let names: Vec<String> = match cfg.mode {
                anodizer_core::config::NpmMode::Postinstall => {
                    vec![crate::npm::manifest::resolve_name(cfg, &crate_name).to_string()]
                }
                anodizer_core::config::NpmMode::OptionalDeps => {
                    let skip_meta = match cfg.skip_metapackage.as_ref() {
                        Some(s) => match s.try_evaluates_to_true(|t| ctx.render_template(t)) {
                            Ok(b) => b,
                            Err(_) => return Ok(ReconcileState::Absent),
                        },
                        None => false,
                    };
                    let has_binaries = !ctx
                        .artifacts
                        .by_kind(anodizer_core::artifact::ArtifactKind::UploadableBinary)
                        .is_empty()
                        || !ctx
                            .artifacts
                            .by_kind(anodizer_core::artifact::ArtifactKind::Binary)
                            .is_empty();
                    if !has_binaries {
                        return Ok(ReconcileState::Absent);
                    }
                    let Ok(layout) = super::optional_deps::generate_layout(
                        ctx,
                        cfg,
                        &crate_name,
                        &version,
                        None,
                        &log,
                    ) else {
                        return Ok(ReconcileState::Absent);
                    };
                    let mut names: Vec<String> =
                        layout.platforms.into_iter().map(|p| p.name).collect();
                    if !skip_meta {
                        names.push(
                            super::optional_deps::resolve_metapackage(cfg, &crate_name).to_string(),
                        );
                    }
                    names
                }
            };
            if names.is_empty() {
                return Ok(ReconcileState::Absent);
            }
            for name in &names {
                let url = format!(
                    "{registry}/{}/{version}",
                    super::auth::encode_package_path(name)
                );
                match crate::publisher_preflight::probe_version_landing(
                    &url,
                    "reconcile: npm version",
                    &policy,
                    deadline,
                    &log,
                ) {
                    Ok(true) => published += 1,
                    Ok(false) => return Ok(ReconcileState::Absent),
                    Err(e) => {
                        return Ok(ReconcileState::Unknown {
                            reason: format!(
                                "npm registry probe failed for {name}@{version}: {e:#}"
                            ),
                        });
                    }
                }
            }
        }
        Ok(ReconcileState::Complete {
            note: format!("all {published} planned npm package(s) already published at {version}"),
        })
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
                .primary_crate_name()
                .map(str::to_string)
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
                ctx,
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
            // creates short-lived publish-only credentials that cannot unpublish.
            // The empty-token skip above already routes OIDC-published packages
            // to the manual-unpublish warning.
            let auth = super::auth::NpmAuth::Token(token);
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

    /// Live pre-publish gate. npm has no companion state-query checker, so this
    /// is its only guard against the two irreversible failure modes:
    ///
    /// * token unusable — it fails to render, or `GET {registry}/-/whoami`
    ///   answers 401/403 ⇒ Blocker under `auth: token`, and under `auth: auto`
    ///   with no OIDC context. Under `auth: auto` *in* an OIDC context it is a
    ///   Warning: every existing package publishes through Trusted Publishing
    ///   and only a brand-new package needs the token, so blocking would abort
    ///   the whole gate — including the sibling publishers that never read it.
    ///   Under `auth: oidc` the token is neither resolved nor probed.
    /// * version already published — `GET {registry}/{pkg}/{version}` 200 ⇒
    ///   Warning (npm forbids republishing a version; unpublish is a 72h window).
    ///
    /// The token probe runs only when a token resolves: an OIDC-only entry has
    /// no long-lived token to validate here (Trusted Publishing creates its
    /// credential at publish time), so probing it would false-block.
    fn preflight(&self, ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        use crate::publisher_preflight::{
            TokenAuth, merge, probe_token_auth, probe_version_published,
        };
        use anodizer_core::PreflightCheck;

        // Shallow probe policy: best-effort pre-publish gate, not a write that
        // must succeed (see `RetryPolicy::PREFLIGHT`).
        let policy = anodizer_core::retry::RetryPolicy::PREFLIGHT;
        let crate_name = ctx
            .config
            .primary_crate_name()
            .map(str::to_string)
            .unwrap_or_else(|| ctx.config.project_name.clone());
        let version = ctx.version();

        let mut acc = PreflightCheck::Pass;
        // The OIDC request env is process-wide, not per-entry, and every arm
        // that grades a token defect keys its severity off it — resolve it once
        // so the arms cannot disagree.
        let oidc_available = super::auth::resolve_oidc_env(ctx).is_some();
        for cfg in active_npm_configs(ctx) {
            acc = merge(
                acc,
                crate::publisher_helpers::targets_allowlist_check(
                    ctx,
                    cfg.targets.as_ref(),
                    cfg.ids.as_ref(),
                    "npm",
                ),
            );
            let Ok(registry) = crate::npm::manifest::resolve_registry(ctx, cfg) else {
                continue;
            };
            // The package this entry publishes under its own name: the
            // postinstall package, or the optional-deps metapackage. Named in
            // every message below so a multi-entry `npms:` says WHICH entry.
            let entry_name = match cfg.mode {
                anodizer_core::config::NpmMode::Postinstall => {
                    crate::npm::manifest::resolve_name(cfg, &crate_name).to_string()
                }
                anodizer_core::config::NpmMode::OptionalDeps => {
                    super::optional_deps::resolve_metapackage(cfg, &crate_name).to_string()
                }
            };
            // Under `auto` in an OIDC context every package that already exists
            // publishes through Trusted Publishing, so an unusable token costs
            // only the brand-new-package fallback. A Blocker there aborts the
            // whole gate and strands the sibling publishers (PyPI, crates.io)
            // that never touch this token.
            let token_defect = |msg: String| -> PreflightCheck {
                if cfg.auth == anodizer_core::config::NpmAuthMode::Auto && oidc_available {
                    PreflightCheck::Warning(format!(
                        "{msg}; existing packages publish via OIDC (Trusted Publishing), a \
                         brand-new package would fail — rotate or remove NPM_TOKEN"
                    ))
                } else {
                    PreflightCheck::Blocker(msg)
                }
            };
            // `oidc` mode never consults a token — `resolve_auth_for_package`
            // resolves none there — so preflight must not resolve one either: a
            // token that fails to render, or one that is simply stale, cannot
            // affect a publish that authenticates through Trusted Publishing.
            if cfg.auth == anodizer_core::config::NpmAuthMode::Oidc {
                ctx.logger("preflight").verbose(&format!(
                    "npm: auth mode is `oidc` for '{entry_name}' on {registry} — a configured \
                     token is ignored and not validated"
                ));
            } else {
                // An empty `Ok` is the legitimate absent-token path (an `auto`
                // entry that authenticates through OIDC): nothing to probe.
                let token = match super::auth::resolve_token(ctx, cfg) {
                    Ok(t) => t,
                    Err(e) => {
                        // A `cfg.token` template that fails to render fails the
                        // live publish in every mode that reads a token, so it
                        // is graded like a dead token rather than deferred past
                        // the tag and the other one-way doors.
                        acc = merge(
                            acc,
                            token_defect(format!(
                                "npm token could not be resolved for '{entry_name}': {e:#}"
                            )),
                        );
                        String::new()
                    }
                };
                if !token.is_empty() {
                    let outcome = match probe_token_auth(
                        &format!("{registry}/-/whoami"),
                        &format!("Bearer {token}"),
                        "preflight: npm whoami",
                        &policy,
                        ctx.retry_deadline(),
                        &ctx.logger("preflight"),
                        &[],
                    ) {
                        TokenAuth::Valid => PreflightCheck::Pass,
                        TokenAuth::Invalid => token_defect(format!(
                            "npm token invalid or expired for '{entry_name}' on {registry}"
                        )),
                        // `--strict` promotes an unverifiable token to a
                        // Blocker, but not where OIDC already covers every
                        // existing package: the same reasoning as an outright
                        // dead token, applied to one that could not be reached.
                        TokenAuth::Indeterminate(reason) => {
                            let msg =
                                format!("could not verify npm token for '{entry_name}' ({reason})");
                            if cfg.auth == anodizer_core::config::NpmAuthMode::Auto
                                && oidc_available
                            {
                                PreflightCheck::Warning(msg)
                            } else {
                                anodizer_core::git::indeterminate_check(
                                    ctx.preflight_is_strict(),
                                    msg,
                                )
                            }
                        }
                    };
                    acc = merge(acc, outcome);
                }
            }
            // Probe the package name(s) this entry will actually publish:
            // * postinstall — the single `name:` package.
            // * optional-deps — the metapackage (resolve_name may differ from
            //   what publish creates), or, when `skip_metapackage` evaluates
            //   truthy, the per-platform packages: the metapackage is owned by
            //   an external pipeline and EXPECTED to exist at this version, so
            //   probing it would false-warn while probing the per-platform
            //   names keeps duplicate-version detection working.
            let names: Vec<String> = match cfg.mode {
                anodizer_core::config::NpmMode::Postinstall => vec![entry_name.clone()],
                anodizer_core::config::NpmMode::OptionalDeps => {
                    let skip_meta = match cfg.skip_metapackage.as_ref() {
                        Some(s) => match s.try_evaluates_to_true(|t| ctx.render_template(t)) {
                            Ok(b) => b,
                            Err(e) => {
                                acc = merge(
                                    acc,
                                    PreflightCheck::Blocker(format!(
                                        "npm skip_metapackage template could not be rendered: {e:#}"
                                    )),
                                );
                                continue;
                            }
                        },
                        None => false,
                    };
                    if skip_meta {
                        // Per-platform names derive from the built artifacts.
                        // Distinguish the two failure classes generate_layout
                        // folds together: "no artifacts yet" (preflight ran
                        // before the build) is a benign skip with a verbose
                        // note; a real layout/config error (unset scope,
                        // colliding template names) is a Blocker that must not
                        // be swallowed into a false-clean preflight.
                        let has_binaries = !ctx
                            .artifacts
                            .by_kind(anodizer_core::artifact::ArtifactKind::UploadableBinary)
                            .is_empty()
                            || !ctx
                                .artifacts
                                .by_kind(anodizer_core::artifact::ArtifactKind::Binary)
                                .is_empty();
                        if !has_binaries {
                            ctx.logger("preflight").verbose(
                                "npm: no binary artifacts yet — skipping the skip_metapackage \
                                 per-platform name probe (names derive from built binaries)",
                            );
                            Vec::new()
                        } else {
                            match super::optional_deps::generate_layout(
                                ctx,
                                cfg,
                                &crate_name,
                                &version,
                                None,
                                &ctx.logger("preflight"),
                            ) {
                                Ok(layout) => {
                                    layout.platforms.into_iter().map(|p| p.name).collect()
                                }
                                Err(e) => {
                                    acc = merge(
                                        acc,
                                        PreflightCheck::Blocker(format!(
                                            "npm optional-deps layout is invalid: {e:#}"
                                        )),
                                    );
                                    continue;
                                }
                            }
                        }
                    } else {
                        vec![entry_name.clone()]
                    }
                }
            };
            for name in &names {
                let url = format!(
                    "{registry}/{}/{version}",
                    super::auth::encode_package_path(name)
                );
                if probe_version_published(
                    &url,
                    "preflight: npm version",
                    &policy,
                    ctx.retry_deadline(),
                    &ctx.logger("preflight"),
                ) {
                    acc = merge(
                        acc,
                        PreflightCheck::Warning(format!(
                            "npm {name}@{version} already published; republish will be rejected"
                        )),
                    );
                }
            }
        }
        Ok(acc)
    }
}

#[cfg(test)]
mod preflight_tests {
    use anodizer_core::Publisher;
    use anodizer_core::config::{NpmAuthMode, NpmConfig};
    use anodizer_core::context::Context;
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::test_helpers::responder::{
        canned_http_response, spawn_oneshot_http_responder,
    };

    /// One `auth: token` entry pointed at the responder, on a sealed (closed,
    /// empty) env so the host's own `NPM_TOKEN` / `ACTIONS_ID_TOKEN_REQUEST_*`
    /// cannot change the auth mode the assertions assume.
    fn ctx_with_npm(registry: String, token: &str) -> Context {
        let mut ctx = TestContextBuilder::new()
            .project_name("proj")
            .sealed_env()
            .build();
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.config.npms = Some(vec![NpmConfig {
            registry: Some(registry),
            token: Some(token.to_string()),
            auth: NpmAuthMode::Token,
            name: Some("pkg".to_string()),
            ..Default::default()
        }]);
        ctx
    }

    /// End-to-end wiring proof: an invalid token must surface through the full
    /// path (config enumeration → registry/token resolution → `/-/whoami`) as a
    /// Blocker. The whoami probe is served 401; the follow-up version probe 404.
    #[test]
    fn npm_preflight_blocks_on_invalid_token() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![
            canned_http_response("401 Unauthorized", ""),
            canned_http_response("404 Not Found", ""),
        ]);
        let ctx = ctx_with_npm(format!("http://{addr}"), "bad-token");
        match super::NpmPublisher::new()
            .preflight(&ctx)
            .expect("preflight ok")
        {
            anodizer_core::PreflightCheck::Blocker(m) => {
                assert!(m.contains("npm token invalid"), "{m}")
            }
            other => panic!("expected Blocker, got {other:?}"),
        }
    }

    /// optional-deps mode publishes the METAPACKAGE name, so preflight must
    /// probe that name — not `resolve_name`'s `name:` — for duplicate
    /// versions. The Warning must cite the metapackage.
    #[test]
    fn npm_preflight_optional_deps_probes_metapackage_name() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![
            canned_http_response("200 OK", r#"{"username":"me"}"#),
            canned_http_response("200 OK", r#"{"name":"meta","version":"1.0.0"}"#),
        ]);
        let mut ctx = ctx_with_npm(format!("http://{addr}"), "good-token");
        let npms = ctx.config.npms.as_mut().expect("npms");
        npms[0].metapackage = Some("meta".to_string());
        match super::NpmPublisher::new()
            .preflight(&ctx)
            .expect("preflight ok")
        {
            anodizer_core::PreflightCheck::Warning(m) => {
                assert!(m.contains("meta@1.0.0"), "must probe the metapackage: {m}")
            }
            other => panic!("expected Warning, got {other:?}"),
        }
    }

    /// With `skip_metapackage` truthy the metapackage is owned by an external
    /// pipeline and EXPECTED to exist — preflight must not probe (and warn
    /// on) its name. With no built artifacts the per-platform names cannot be
    /// derived yet, so the version probe is skipped entirely: only the whoami
    /// probe fires and the check passes.
    #[test]
    fn npm_preflight_skip_metapackage_skips_metapackage_probe() {
        // Only the whoami response is provisioned; a metapackage version
        // probe would hit a closed responder and surface as a Warning/probe
        // noise instead of the clean Pass asserted here.
        let (addr, _c) = spawn_oneshot_http_responder(vec![canned_http_response(
            "200 OK",
            r#"{"username":"me"}"#,
        )]);
        let mut ctx = ctx_with_npm(format!("http://{addr}"), "good-token");
        let npms = ctx.config.npms.as_mut().expect("npms");
        npms[0].skip_metapackage = Some(anodizer_core::config::StringOrBool::Bool(true));
        match super::NpmPublisher::new()
            .preflight(&ctx)
            .expect("preflight ok")
        {
            anodizer_core::PreflightCheck::Pass => {}
            other => panic!("expected Pass, got {other:?}"),
        }
    }

    /// A valid token + an already-published version must surface as a Warning
    /// (not a Blocker): the publish would be rejected but the credential is good.
    #[test]
    fn npm_preflight_warns_on_already_published() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![
            canned_http_response("200 OK", r#"{"username":"me"}"#),
            canned_http_response("200 OK", r#"{"name":"pkg","version":"1.0.0"}"#),
        ]);
        let ctx = ctx_with_npm(format!("http://{addr}"), "good-token");
        match super::NpmPublisher::new()
            .preflight(&ctx)
            .expect("preflight ok")
        {
            anodizer_core::PreflightCheck::Warning(m) => {
                assert!(m.contains("already published"), "{m}")
            }
            other => panic!("expected Warning, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod config_fully_inactive_tests {
    use anodizer_core::Publisher;
    use anodizer_core::config::{Config, NpmConfig, StringOrBool};
    use anodizer_core::context::{Context, ContextOptions};

    fn ctx_with_npms(npms: Vec<NpmConfig>) -> Context {
        let config = Config {
            project_name: "proj".to_string(),
            npms: Some(npms),
            ..Default::default()
        };
        Context::new(config, ContextOptions::default())
    }

    /// Every configured `npms[]` entry is `skip: true` — no entry is
    /// active, so the publisher must report fully-inactive rather than
    /// falling through to `run()` and recording a zero-evidence
    /// `Succeeded`, which would falsely burn the npm slot for rollback
    /// purposes and let a required-npm-but-all-skipped config pass the
    /// required-failures gate silently.
    #[test]
    fn config_fully_inactive_true_when_all_entries_inactive() {
        let npm = NpmConfig {
            name: Some("pkg".to_string()),
            skip: Some(StringOrBool::Bool(true)),
            ..Default::default()
        };
        let ctx = ctx_with_npms(vec![npm]);

        assert!(
            super::NpmPublisher::new().config_fully_inactive(&ctx),
            "every npms[] entry is skip:true; the publisher must be fully inactive"
        );
    }

    /// `npms` is a top-level list (not crate-scoped), so an active entry
    /// with no `--crate` filter applied must keep the publisher live —
    /// mirrors `uploads.rs`'s
    /// `config_fully_inactive_false_with_empty_selection_and_active_entry`.
    #[test]
    fn config_fully_inactive_false_with_empty_selection_and_active_entry() {
        let npm = NpmConfig {
            name: Some("pkg".to_string()),
            ..Default::default()
        };
        let ctx = ctx_with_npms(vec![npm]);

        assert!(
            !super::NpmPublisher::new().config_fully_inactive(&ctx),
            "an active npms[] entry must keep the publisher live"
        );
    }
}

#[cfg(test)]
mod sealed_env_pin {
    use anodizer_core::test_helpers::test_sources::{
        function_bodies, production_half, rust_sources, test_sources,
    };
    use std::path::Path;

    /// Whether a body closes its context's env source. `sealed_env()` does it
    /// explicitly, and a `TestContextBuilder` `.env(...)` override does it as a
    /// side effect (a non-empty override list swaps to a closed map). `.env(`
    /// on its own proves nothing: `Command::env` spells the same call on a
    /// subprocess and seals no context.
    fn seals(body: &str) -> bool {
        body.contains("sealed_env()")
            || (body.contains("TestContextBuilder") && body.contains(".env("))
    }

    /// Whether a body drives a path that reads credentials or runner-detection
    /// variables: a publisher `preflight`, or a `run` handed a `&mut` context
    /// (the receiver and the binding name vary, so neither is matched exactly).
    ///
    /// Asked only of a function that takes no parameters, which is every
    /// `#[test]` — the predicates of this pin quote the same call spellings and
    /// would otherwise report themselves.
    fn reads_env(head: &str, body: &str) -> bool {
        head.contains("()")
            && (body.contains(".preflight(") || (body.contains(".run(") && body.contains("&mut")))
    }

    /// Names of the helper functions in one source that build a sealed context,
    /// so a test delegating its setup to one is accepted.
    fn sealing_helpers(bodies: &[String]) -> Vec<String> {
        bodies
            .iter()
            .filter(|b| seals(b))
            .filter_map(|b| {
                let head = b.lines().next()?.trim_start();
                let name = head.split("fn ").nth(1)?.split('(').next()?;
                Some(name.to_string())
            })
            .collect()
    }

    /// Every test-carrying region under `src/npm`: a whole test source, and the
    /// inline test module of a production source — `publisher.rs` keeps its
    /// preflight tests inline, and a walk of `test_sources` alone never saw
    /// them.
    fn test_regions(dir: &Path) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for path in test_sources(dir) {
            let src = std::fs::read_to_string(&path).expect("read test source");
            out.push((path.display().to_string(), src));
        }
        for path in rust_sources(dir) {
            let src = std::fs::read_to_string(&path).expect("read production source");
            let inline = src[production_half(&src).len()..].to_string();
            if !inline.is_empty() {
                out.push((path.display().to_string(), inline));
            }
        }
        out
    }

    /// `preflight` and `run` read credentials and runner-detection variables.
    /// A test whose context falls through to the process env probes the real
    /// registry on any machine that exports `NPM_TOKEN`, so it passes on a
    /// laptop and fails on a runner (and the other way round).
    #[test]
    fn every_npm_preflight_test_seals_its_env() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/npm");
        let mut unsealed = Vec::new();
        let mut total = 0usize;
        for (label, text) in test_regions(&dir) {
            let bodies = function_bodies(&text);
            let helpers = sealing_helpers(&bodies);
            for body in &bodies {
                let head = body.lines().next().unwrap_or_default().trim().to_string();
                if !reads_env(&head, body) {
                    continue;
                }
                total += 1;
                let sealed = seals(body) || helpers.iter().any(|h| body.contains(&format!("{h}(")));
                if !sealed {
                    unsealed.push(format!("{label}: {head}"));
                }
            }
        }
        assert!(
            total >= 24,
            "the walk found only {total} preflight/run tests; it stopped seeing the population"
        );
        assert!(
            unsealed.is_empty(),
            "an npm preflight/run test must build its context with sealed_env() (or seed it \
             with TestContextBuilder::env(...)) so it never reads the ambient NPM_TOKEN; \
             unsealed: {unsealed:?}"
        );
    }
}
