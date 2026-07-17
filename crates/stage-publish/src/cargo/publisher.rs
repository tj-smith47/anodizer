//! The [`CargoPublisher`] [`anodizer_core::Publisher`] adapter, its
//! operator-visible run messages, and yank-target evidence encoding.

use super::*;

// ---------------------------------------------------------------------------
// CargoPublisher - Publisher trait adapter
// ---------------------------------------------------------------------------

// Publisher trait adapter around `publish_to_cargo`. Classified as
// `Submitter` + `required=true`: crates.io publish is effectively one-way
// (versions cannot be re-uploaded), so a failure here must fail the release
// and other Submitter publishers must already be gated.
/// The crate-level `publish.cargo` block — the single accessor the
/// registry gate and the gate-override collapse key on.
pub(crate) fn block(
    p: &anodizer_core::config::PublishConfig,
) -> Option<&anodizer_core::config::CargoPublishConfig> {
    p.cargo.as_ref()
}

simple_publisher!(
    CargoPublisher,
    "cargo",
    anodizer_core::PublisherGroup::Submitter,
    true,
    Some("CARGO_REGISTRY_TOKEN yank"),
);

/// Operator-visible start line for the cargo publisher. Worded apart from
/// [`crate::publisher_helpers::run_start_message`] on purpose: cargo
/// processes every selected crate rather than scanning them for a config
/// block, so "scanning … for a cargo config block" would misdescribe it.
pub(crate) fn run_start_message(selected_total: usize) -> String {
    format!(
        "starting cargo publish — processing {} selected crate(s)",
        selected_total
    )
}

/// Operator-visible per-crate start line. Emitted by `publish_to_cargo`
/// immediately before each crate's publish-or-skip decision so the
/// per-crate progress is anchored to a specific name in the log.
/// Mirrors `run_per_crate_start_message` on every other per-crate
/// publisher (homebrew, scoop, nix, aur, krew).
pub(crate) fn run_per_crate_start_message(crate_name: &str) -> String {
    format!("starting per-crate cargo publish for '{}'", crate_name)
}

/// Operator-visible done line, emitted after `publish_to_cargo` returns
/// Ok. `processed` counts crates whose publish path was actually
/// invoked (skipped-by-already-published, skipped-by-skip-template, and
/// dry-run paths all count as processed — they're successful runs of
/// the correct code path).
pub(crate) fn run_done_message(processed: usize) -> String {
    format!(
        "finished cargo publish — {} selected crate(s) processed",
        processed
    )
}

/// Warning emitted when the publisher was registered (at least one
/// crate has a `publish.cargo` block) but `publish_to_cargo` resolved
/// zero publishable crates (every cargo-configured crate was filtered
/// out by `--crate` / `--all` selection).
pub(crate) fn run_no_eligible_crates_warning(selected_total: usize) -> String {
    format!(
        "cargo publisher registered but 0 of {} effective crate(s) had a cargo \
         config block — nothing pushed. Check that --crate / --all selects a \
         crate whose publish.cargo block is set.",
        selected_total
    )
}

/// Cargo entries across the crate universe whose `skip:`/`if:` evaluates
/// active right now AND whose crate is in scope for `--crate` / `--all`
/// selection. Shared by [`anodizer_core::Publisher::requirements`],
/// [`anodizer_core::Publisher::config_fully_inactive`], the publisher's
/// `run()` eligible-count, `preflight()`'s crates.io-probe gate, and
/// `super::publish::resolve_workspace_cargo_token`'s credential-decision
/// ladder — so none of them can diverge on which crates are actually
/// reachable this invocation — the same selection semantics as
/// [`crate::publisher_helpers::effective_publish_crates`] (empty selection =
/// every crate; non-empty = exactly those names), applied before the
/// skip/if filter so a selected-but-skipped crate cannot masquerade as
/// active via an out-of-scope sibling.
pub(crate) fn active_cargo_configs(
    ctx: &Context,
) -> Vec<&anodizer_core::config::CargoPublishConfig> {
    let selected = &ctx.options.selected_crates;
    ctx.config
        .crate_universe()
        .into_iter()
        .filter(|c| selected.is_empty() || selected.iter().any(|s| s == &c.name))
        .filter_map(|c| c.publish.as_ref()?.cargo.as_ref())
        .filter(|cargo| {
            !crate::publisher_helpers::entry_inactive(
                ctx,
                cargo.skip.as_ref(),
                None,
                cargo.if_condition.as_deref(),
            )
        })
        .collect()
}

impl anodizer_core::Publisher for CargoPublisher {
    fn name(&self) -> &str {
        Self::PUBLISHER_NAME
    }

    fn group(&self) -> anodizer_core::PublisherGroup {
        Self::PUBLISHER_GROUP
    }

    fn required(&self) -> bool {
        Self::resolved_required(self)
    }

    fn skips_on_nightly(&self) -> bool {
        true
    }

    fn config_fully_inactive(&self, ctx: &Context) -> bool {
        active_cargo_configs(ctx).is_empty()
    }

    fn requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        // `cargo publish` runs the literal `cargo` from PATH, so the tool is
        // always required when a cargo block is active. The credential gate is
        // per the block's `auth` mode, mirroring the pypi publisher.
        use anodizer_core::config::CargoAuthMode;
        let active: Vec<_> = active_cargo_configs(ctx);
        if active.is_empty() {
            return Vec::new();
        }
        let mut out = vec![anodizer_core::EnvRequirement::Tool {
            name: "cargo".to_string(),
        }];
        let oidc_vars = || -> Vec<String> {
            super::oidc::OIDC_ENV_VARS
                .iter()
                .map(|s| s.to_string())
                .collect()
        };
        for cargo in &active {
            match cargo.resolved_auth() {
                // Token-only: the crates.io token is mandatory.
                CargoAuthMode::Token => out.push(anodizer_core::EnvRequirement::EnvAllOf {
                    vars: vec!["CARGO_REGISTRY_TOKEN".to_string()],
                }),
                // Strict OIDC: require the GitHub Actions request pair; never a
                // token (the run path refuses to fall back to one).
                CargoAuthMode::Oidc => {
                    out.push(anodizer_core::EnvRequirement::EnvAllOf { vars: oidc_vars() })
                }
                // Auto resolves at publish time (token if present, else OIDC).
                // Preflight applies only a COARSE token-OR-OIDC gate so it
                // catches the zero-credential case without false-failing a
                // valid OIDC-only run (which has id-token: write but no stored
                // CARGO_REGISTRY_TOKEN).
                CargoAuthMode::Auto => {
                    let mut any = vec!["CARGO_REGISTRY_TOKEN".to_string()];
                    any.extend(oidc_vars());
                    out.push(anodizer_core::EnvRequirement::EnvAnyOf { vars: any });
                }
            }
        }
        out
    }

    fn programmatic_rollback_on_failure(&self, evidence: &anodizer_core::PublishEvidence) -> bool {
        // A failed cargo run that already pushed one or more crates to
        // crates.io recorded them here; rollback must yank them even
        // though the overall outcome is `Failed`. An empty record means
        // nothing went live — keep the failure inert.
        !decode_cargo_yank_targets(&evidence.extra).is_empty()
    }

    fn retain_on_rollback(&self) -> bool {
        Self::resolved_retain_on_rollback(self)
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        let log = ctx.logger("publish");
        let selected = ctx.options.selected_crates.clone();
        // Operator-facing visible-work bookends — every per-crate publisher
        // emits these so a no-op dispatch can't masquerade as success.
        // `publish_to_cargo` emits per-crate progress
        // (`(dry-run) would run: ...` / `running: cargo publish -p ...` /
        // `skipped ... already published`) plus the per-crate-start line
        // from `run_per_crate_start_message` which forms the loop-body
        // signal that satisfies the visible-work contract.
        let eligible = active_cargo_configs(ctx).len();
        log.status(&run_start_message(eligible.max(selected.len())));
        // Short-circuit BEFORE delegating into publish_to_cargo when no
        // cargo-configured crate is eligible — otherwise the inner path
        // would also emit a "no crates configured ..." status, duplicating
        // the canonical no-eligible warn the wrapper owns.
        if eligible == 0 {
            log.warn(&run_no_eligible_crates_warning(selected.len()));
            return Ok(anodizer_core::PublishEvidence::new("cargo"));
        }
        // `record` accumulates one entry per crate whose `cargo publish`
        // actually succeeds. On the failure path we still build evidence
        // from whatever was published before the bail and stash it on the
        // context so dispatch can hand it to rollback — otherwise a
        // partial multi-crate publish would leave the succeeded crates
        // live with nothing to yank.
        let mut record: Vec<CargoYankTarget> = Vec::new();
        let publish_result = publish_to_cargo(ctx, &selected, &log, &mut record);

        let mut evidence = anodizer_core::PublishEvidence::new("cargo");
        if let Some(primary) = first_published_crate(ctx) {
            evidence.primary_ref = Some(format!(
                "https://crates.io/crates/{name}/{version}",
                name = primary.name,
                version = primary.version
            ));
        }
        evidence.extra = encode_cargo_yank_targets(&record);

        match publish_result {
            Ok(()) => {
                log.status(&run_done_message(eligible));
                Ok(evidence)
            }
            Err(e) => {
                // Stash the partial evidence BEFORE propagating so the
                // dispatcher's `Err` arm can recover it for rollback.
                ctx.record_pending_evidence(evidence);
                Err(e)
            }
        }
    }

    fn rollback(
        &self,
        ctx: &mut Context,
        evidence: &anodizer_core::PublishEvidence,
    ) -> anyhow::Result<()> {
        let log = ctx.logger("publish");
        // Yank from the authoritative record built at publish time: each
        // entry is a crate whose `cargo publish` actually SUCCEEDED this
        // run, with the per-crate version and the registry/index the
        // publish used. This is correct even when the local `.crate`
        // files are gone (workspace cleaned, different CI job, run died
        // before packaging) — the old disk-scan rollback yanked NOTHING in
        // that case, leaving succeeded crates live.
        let targets = decode_cargo_yank_targets(&evidence.extra);
        if targets.is_empty() {
            // Nothing was published this run — a clean no-op, not a
            // failure to recover. (Verbose, not a scary warn: an empty
            // record is the normal shape when the failing publisher never
            // reached its first successful `cargo publish`.)
            log.verbose("no crates published this run; cargo rollback is a no-op");
            return Ok(());
        }
        let mut yanked = 0usize;
        let mut failed = 0usize;
        if ctx.is_dry_run() {
            log.status(&format!(
                "(dry-run) would yank {} crate(s) from their configured registries",
                targets.len()
            ));
            return Ok(());
        }
        // Credential for the yank: the overlaid minted token under `auth: oidc`
        // (installed by `publish_to_cargo` and left live for this rollback), or
        // the ambient `CARGO_REGISTRY_TOKEN` under `auth: token`. Read through
        // the context env source so both paths resolve uniformly; injected via
        // env, never argv.
        let registry_token = ctx.env_var("CARGO_REGISTRY_TOKEN");
        for t in &targets {
            // crates.io versions are immutable, so `cargo yank` is the
            // strongest unwind available; the version slot stays burned
            // and any consumer that already resolved against it keeps
            // working. Operators must still bump to recover.
            let (args, env_pair) = build_yank_invocation(t, registry_token.as_deref());
            let target = t
                .registry
                .as_deref()
                .or(t.index.as_deref())
                .unwrap_or("crates.io");
            log.status(&format!("yanking {} {} ({})", t.name, t.version, target));
            let mut command = Command::new("cargo");
            command.args(&args);
            if let Some((k, v)) = &env_pair {
                command.env(k, v);
            }
            let output = command.output()?;
            if output.status.success() {
                yanked += 1;
            } else {
                failed += 1;
                log.warn(&format!(
                    "cargo yank failed for {} {} on {}: {}",
                    t.name,
                    t.version,
                    target,
                    String::from_utf8_lossy(&output.stderr),
                ));
            }
        }
        log.status(&format!(
            "cargo rollback yanked {} crate(s), {} failure(s)",
            yanked, failed
        ));

        // A minted OIDC token was kept live so this yank could run; revoke it
        // (best-effort) now that the unwind is done, and restore the base env
        // source. Under `auth: token` the ambient token is the operator's
        // long-lived credential — `end_cargo_trusted_publishing` returns `None`
        // and it is neither revoked nor disturbed.
        if let Some(token) = ctx.end_cargo_trusted_publishing() {
            let policy = ctx.retry_policy();
            super::oidc::revoke_trusted_publishing_token(&token, &policy, &log);
        }
        Ok(())
    }

    fn preflight(&self, ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        // Token VALIDITY only — duplicate-version and partial-publish are
        // already caught by the state-query checker + `cargo publish
        // --dry-run`. `requirements()` gates token PRESENCE; this proves the
        // present token is accepted before the irreversible first publish.
        //
        // crates.io resolves the Authorization token BEFORE applying endpoint
        // policy, so `/api/v1/me` still authenticates a token it then refuses
        // to serve: an unknown/expired token gets `authentication failed`,
        // while a real token on this now cookie-only route (or one restricted
        // by endpoint scopes) gets a distinct policy-denial body. Those
        // denials therefore PROVE validity and must not block the release as a Blocker —
        // least-privilege scoped tokens are the recommended shape for CI.
        // Only probe crates.io when an ACTIVE cargo publisher targets the
        // default registry. An entry with `registry:`/`index:` set publishes
        // to a private registry whose credential is `CARGO_REGISTRIES_<NAME>_TOKEN`,
        // NOT the `CARGO_REGISTRY_TOKEN` this probe presents to
        // `crates.io/api/v1/me` — probing crates.io for it would false-Blocker a
        // perfectly valid private-registry release. Holds across single-crate,
        // lockstep, and per-crate modes (per-crate entries may each pick a
        // different registry).
        let probes_crates_io = active_cargo_configs(ctx)
            .into_iter()
            .any(|cargo| cargo.registry.is_none() && cargo.index.is_none());
        if !probes_crates_io {
            return Ok(anodizer_core::PreflightCheck::Pass);
        }
        let token = ctx
            .env_source()
            .var("CARGO_REGISTRY_TOKEN")
            .unwrap_or_default();
        if token.is_empty() {
            return Ok(anodizer_core::PreflightCheck::Pass);
        }
        let policy = anodizer_core::retry::RetryPolicy::PREFLIGHT;
        let api_url = format!("{}/api/v1/me", crates_io_api_base());
        Ok(
            match crate::publisher_preflight::probe_token_auth(
                &api_url,
                &token,
                "preflight: crates.io token",
                &policy,
                &ctx.logger("preflight"),
                CRATES_IO_AUTHENTICATED_DENIALS,
            ) {
                crate::publisher_preflight::TokenAuth::Valid => anodizer_core::PreflightCheck::Pass,
                crate::publisher_preflight::TokenAuth::Invalid => {
                    anodizer_core::PreflightCheck::Blocker("crates.io token invalid".into())
                }
                crate::publisher_preflight::TokenAuth::Indeterminate(reason) => {
                    anodizer_core::git::indeterminate_check(
                        ctx.preflight_is_strict(),
                        format!("could not verify crates.io token ({reason})"),
                    )
                }
            },
        )
    }

    fn rollback_scope_needed(&self) -> Option<&'static str> {
        Self::ROLLBACK_SCOPE
    }
}

/// Build the `cargo yank` argv and the optional `CARGO_REGISTRY_TOKEN` env
/// pair for one recorded target.
///
/// The credential is injected via ENV, never argv — a `--token` argument would
/// expose it in the process list. A `None` / empty `token` yields no env pair,
/// so the yank inherits the ambient environment (byte-identical to the historic
/// `Command::new("cargo").args(...)` behaviour). Split out from
/// [`CargoPublisher::rollback`] so the argv + env construction is unit-testable
/// without a live registry.
pub(crate) fn build_yank_invocation(
    t: &CargoYankTarget,
    token: Option<&str>,
) -> (Vec<String>, Option<(String, String)>) {
    let mut args: Vec<String> = vec![
        "yank".into(),
        "--version".into(),
        t.version.clone(),
        t.name.clone(),
    ];
    if let Some(ref r) = t.registry {
        args.push("--registry".into());
        args.push(r.clone());
    }
    if let Some(ref idx) = t.index {
        args.push("--index".into());
        args.push(idx.clone());
    }
    let env = token
        .filter(|s| !s.is_empty())
        .map(|s| ("CARGO_REGISTRY_TOKEN".to_string(), s.to_string()));
    (args, env)
}

/// Authoritative per-crate record of a `cargo publish` that SUCCEEDED
/// during this run. Aliased to the core-owned snapshot so the evidence
/// schema lives in [`anodizer_core::publish_evidence`] and no
/// credential-shaped field can land in it.
pub(crate) type CargoYankTarget = anodizer_core::publish_evidence::CargoYankTargetSnapshot;

/// Encode the recorded yank targets into the typed
/// [`PublishEvidenceExtra::Cargo`] variant.
pub(crate) fn encode_cargo_yank_targets(
    targets: &[CargoYankTarget],
) -> anodizer_core::PublishEvidenceExtra {
    anodizer_core::PublishEvidenceExtra::Cargo(anodizer_core::publish_evidence::CargoExtra {
        cargo_yank_targets: targets.to_vec(),
    })
}

/// Decode the typed Cargo variant into the recorded yank targets.
/// Returns an empty vec for any other variant — rollback then treats the
/// run as "nothing published this run" and no-ops cleanly.
pub(crate) fn decode_cargo_yank_targets(
    extra: &anodizer_core::PublishEvidenceExtra,
) -> Vec<CargoYankTarget> {
    match extra {
        anodizer_core::PublishEvidenceExtra::Cargo(c) => c.cargo_yank_targets.clone(),
        _ => Vec::new(),
    }
}
