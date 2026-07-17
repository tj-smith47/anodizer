//! `ChocolateyPublisher` — Submitter-group `Publisher` impl wrapping the
//! per-crate [`publish_to_chocolatey`](super::publish_to_chocolatey)
//! entrypoint.
//!
//! Chocolatey is structurally a Submitter publisher: the push to the
//! community feed lands the package in a **moderation queue** at
//! `community.chocolatey.org/packages/<id>`. There is no public
//! programmatic withdraw endpoint. The community gallery's "Maintain"
//! UI is the only path back, and only the package owner can drive it.
//!
//! "Submitter group, no-rollback" contract for chocolatey: record
//! `(crate_name, package_id, version)` tuples in
//! [`anodizer_core::PublishEvidence::extra`] so a `--rollback-only`
//! invocation can surface the exact package page the operator needs to
//! address manually. The `rollback` method itself is warn-only and does
//! not call out to the gallery.
//!
//! CREDENTIAL HANDLING: [`ChocolateyTarget`] stores no auth material.
//! The chocolatey API key (resolved from `publish.chocolatey.api_key`
//! or the `CHOCOLATEY_API_KEY` env var at publish time) is irrelevant
//! to rollback — the manual withdraw flow runs through the community
//! web UI under the package owner's account, not via the push API key
//! — so persisting it into evidence would only leak a credential with
//! no operator benefit.

use anodizer_core::context::Context;

simple_publisher!(
    ChocolateyPublisher,
    "chocolatey",
    anodizer_core::PublisherGroup::Submitter,
    false,
    // Chocolatey's rollback is operator-driven via the community web UI;
    // no env-var credential applies. Naming a token scope here would be
    // misleading — the API key feeds the *push*, not the *withdraw*.
    None,
);

/// Serialized shape of a recorded chocolatey publish. One entry per crate
/// whose publish path successfully submitted to the community feed.
///
/// `package_id` is the rendered nuspec `<id>` (the URL slug on
/// community.chocolatey.org); `version` is the bare semver string
/// Aliased to the core-owned snapshot so the evidence schema lives in
/// [`anodizer_core::publish_evidence`] and credential-shaped fields
/// (`api_key`, `token`, `password`) have no slot to land in.
type ChocolateyTarget = anodizer_core::publish_evidence::ChocolateyTargetSnapshot;

/// Decode the `chocolatey_targets` array from
/// [`anodizer_core::PublishEvidence::extra`]. Rollback treats
/// empty-decode the same as no-evidence and emits the canonical
/// empty-evidence warn.
fn decode_chocolatey_targets(extra: &anodizer_core::PublishEvidenceExtra) -> Vec<ChocolateyTarget> {
    match extra {
        anodizer_core::PublishEvidenceExtra::Chocolatey(c) => c.chocolatey_targets.clone(),
        _ => Vec::new(),
    }
}

/// The crate-level `publish.chocolatey` block — the single accessor the
/// registry gate, the gate-override collapse, and the per-crate dispatch
/// predicate all key on.
pub(crate) fn block(
    p: &anodizer_core::config::PublishConfig,
) -> Option<&anodizer_core::config::ChocolateyConfig> {
    p.chocolatey.as_ref()
}

pub(crate) fn is_chocolatey_per_crate_configured(ctx: &Context, crate_name: &str) -> bool {
    crate::publisher_helpers::is_per_crate_block_configured(ctx, crate_name, block)
}

/// Build a [`ChocolateyTarget`] for the given crate. Reads config + the
/// live process version so the recorded coordinates match what
/// `publish_to_chocolatey` will push. Returns `None` when no chocolatey
/// block is configured (matches the publish path's skip semantics).
fn collect_chocolatey_target(ctx: &Context, crate_name: &str) -> Option<ChocolateyTarget> {
    let c = crate::util::find_crate_in_universe(ctx, crate_name)?;
    let cfg = c.publish.as_ref().and_then(|p| p.chocolatey.as_ref())?;
    let package_id = cfg.name.as_deref().unwrap_or(crate_name).to_string();
    Some(ChocolateyTarget {
        target: package_id.clone(),
        crate_name: crate_name.to_string(),
        package_id,
        version: ctx.version(),
    })
}

/// Message emitted just before delegating to `publish_to_chocolatey`.
/// Anchors the choco activity (nuspec generation, nupkg creation, push)
/// to a specific crate in the log so multi-crate workspaces are
/// disambiguatable.
pub(crate) fn run_per_crate_start_message(crate_name: &str) -> String {
    format!("starting per-crate chocolatey publish for '{}'", crate_name)
}

/// Final summary emitted at publisher exit. `processed` is the count of
/// crates the publisher actually invoked `publish_to_chocolatey` on (not
/// the count of successful pushes — `publish_to_chocolatey` has its own
/// skip paths for moderation/hash-match/dry-run/etc., each of which logs
/// its own status line).
pub(crate) fn run_done_message(processed: usize) -> String {
    format!(
        "finished chocolatey publish — {} configured crate(s) processed",
        processed
    )
}

/// Warning emitted when the publisher was registered (at least one
/// crate has a `publish.chocolatey` block at the config level) but the
/// run path processed zero crates.
///
/// With the implicit-all default in
/// [`crate::publisher_helpers::effective_publish_crates`], an empty
/// `selected_crates` resolves to every crate carrying a
/// `publish.chocolatey` block — so a zero-processed run means
/// `--crate`/`--all` matrix selection was non-empty AND filtered every
/// chocolatey-configured crate out. Operators must see this — otherwise
/// the publisher's `succeeded` status hides the fact that nothing was
/// pushed.
pub(crate) fn run_no_eligible_crates_warning(selected_total: usize) -> String {
    format!(
        "chocolatey publisher registered but 0 of {} effective crate(s) had a chocolatey \
         config block — nothing pushed. Check that --crate / --all selects a \
         crate whose publish.chocolatey block is set.",
        selected_total
    )
}

/// Chocolatey entries across the crate universe whose `skip:`/`if:`
/// evaluates active right now AND whose crate is in scope for `--crate` /
/// `--all` selection (same semantics as
/// [`crate::publisher_helpers::effective_publish_crates`]: empty selection
/// = every crate; non-empty = exactly those names). Shared by
/// [`anodizer_core::Publisher::requirements`] and
/// [`anodizer_core::Publisher::config_fully_inactive`] so the two cannot
/// diverge, and so a selected-but-skipped crate cannot masquerade as active
/// via an out-of-scope sibling. `preflight` keeps its own loop (it needs
/// per-entry feed-URL resolution alongside the filter, not just a boolean).
fn active_chocolatey_configs(ctx: &Context) -> Vec<&anodizer_core::config::ChocolateyConfig> {
    let selected = &ctx.options.selected_crates;
    ctx.config
        .crate_universe()
        .into_iter()
        .filter(|c| selected.is_empty() || selected.iter().any(|s| s == &c.name))
        .filter_map(|c| c.publish.as_ref()?.chocolatey.as_ref())
        .filter(|ch| {
            !crate::publisher_helpers::entry_inactive(
                ctx,
                ch.skip.as_ref(),
                None,
                ch.if_condition.as_deref(),
            )
        })
        .collect()
}

impl anodizer_core::Publisher for ChocolateyPublisher {
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

    fn config_fully_inactive(&self, ctx: &Context) -> bool {
        active_chocolatey_configs(ctx).is_empty()
    }

    fn retain_on_rollback(&self) -> bool {
        Self::resolved_retain_on_rollback(self)
    }

    fn requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        active_chocolatey_configs(ctx)
            .into_iter()
            .flat_map(|ch| {
                // `xmllint` is MANDATORY at gate time: the strict pre-publish
                // schema floor runs `xmllint --schema` against the rendered
                // nuspec and FAILS when the tool is absent (moderation
                // submission is a one-way door — see
                // `schema_validation::chocolatey`). Declared REQUIRED (not
                // advisory) — the caller wraps publisher requirements via
                // `SourcedRequirement::new`, the same frame that makes
                // `git`/`npm` hard requirements — so preflight and
                // `anodizer tools` provision it up front instead of the run
                // dying at the guard after the GitHub release shipped.
                let mut reqs = vec![anodizer_core::EnvRequirement::Tool {
                    name: "xmllint".to_string(),
                }];
                // Mirrors `resolve_api_key`: templated `api_key` from config,
                // else the CHOCOLATEY_API_KEY env var. The push itself is
                // plain HTTPS (no choco CLI).
                if let Some(req) = crate::publisher_helpers::secret_requirement(
                    ch.api_key.as_deref(),
                    "CHOCOLATEY_API_KEY",
                ) {
                    reqs.push(req);
                }
                reqs
            })
            .collect()
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        let log = ctx.logger("publish");
        let mut targets: Vec<ChocolateyTarget> = Vec::new();
        let selected = crate::publisher_helpers::effective_publish_crates(
            ctx,
            is_chocolatey_per_crate_configured,
        );
        log.status(&crate::publisher_helpers::run_start_message(
            "chocolatey",
            selected.len(),
        ));
        // `processed` counts configured crates the loop ENTERED (post
        // implicit-all filter, post `is_chocolatey_per_crate_configured`
        // defensive guard). It is incremented BEFORE
        // `publish_to_chocolatey` runs, so it includes crates whose
        // publish path returned Err — the `?` short-circuits the run
        // without decrementing. The done/no-eligible log uses it to
        // distinguish "no eligible crate selected" (= 0) from "tried
        // at least one" (≥ 1). `targets` tracks actual pushes
        // separately so rollback evidence can't lie about what was
        // submitted.
        let mut processed = 0usize;
        for crate_name in &selected {
            // Defensive guard for explicit `--crate=X` selection when X has no
            // publisher block; implicit-all is already filtered by effective_publish_crates above.
            if !is_chocolatey_per_crate_configured(ctx, crate_name) {
                log.skip_line(
                    ctx.options.show_skipped,
                    &crate::publisher_helpers::no_config_block_message("chocolatey", crate_name),
                );
                continue;
            }
            processed += 1;
            log.verbose(&run_per_crate_start_message(crate_name));
            // Re-scope the version/name template vars to THIS crate's own tag so
            // the rendered nuspec — AND the recorded target version — carry the
            // crate's version, not the first crate's (workspace per-crate
            // independent-version mode).
            //
            // Snapshot the target shape BEFORE the publish path runs (inside the
            // same scope) so a mid-publish failure still leaves the operator a
            // manual withdrawal pointer whose version matches what is pushed —
            // but only commit the snapshot if the publish actually pushed
            // (returns Ok(true)). Recording a target for a skipped run produces
            // a misleading "manual withdrawal required" warning at rollback time
            // for a package this run never submitted.
            let (pushed, snapshot) = crate::publisher_helpers::with_published_crate_scope(
                ctx,
                crate_name,
                &anodizer_core::crate_scope::resolve_crate_tag,
                |ctx| {
                    let snapshot = collect_chocolatey_target(ctx, crate_name);
                    let pushed = super::publish::publish_to_chocolatey(ctx, crate_name, &log)?;
                    Ok((pushed, snapshot))
                },
            )?;
            if pushed && let Some(t) = snapshot {
                targets.push(t);
            }
        }
        if processed == 0 {
            log.warn(&run_no_eligible_crates_warning(selected.len()));
        } else {
            log.status(&run_done_message(processed));
        }
        let mut evidence = anodizer_core::PublishEvidence::new("chocolatey");
        if let Some(first) = targets.first() {
            evidence.primary_ref = Some(format!(
                "https://community.chocolatey.org/packages/{}",
                first.package_id
            ));
        }
        evidence.extra = anodizer_core::PublishEvidenceExtra::Chocolatey(
            anodizer_core::publish_evidence::ChocolateyExtra {
                chocolatey_targets: targets,
            },
        );
        Ok(evidence)
    }

    fn rollback(
        &self,
        ctx: &mut Context,
        evidence: &anodizer_core::PublishEvidence,
    ) -> anyhow::Result<()> {
        let log = ctx.logger("publish");
        let targets = decode_chocolatey_targets(&evidence.extra);
        if targets.is_empty() {
            log.warn(&crate::publisher_helpers::rollback_empty_warning_msg(
                "chocolatey",
                "submitted packages",
            ));
            return Ok(());
        }
        // Chocolatey has no programmatic withdraw endpoint. Surface a
        // warn per recorded target with the exact gallery URL the
        // operator needs to address. This is intentionally NOT an
        // error: a failed automated rollback should not gate the rest
        // of the pipeline.
        for t in &targets {
            log.warn(&format!(
                "manual chocolatey withdrawal required for '{}' version '{}'; \
                 visit https://community.chocolatey.org/packages/{} and use the \
                 'Maintain' UI to withdraw the submission (only the package \
                 owner can drive this; the push API key does not authorize \
                 withdraws).",
                t.package_id, t.version, t.package_id
            ));
        }
        log.status(&format!(
            "{} chocolatey package(s) require manual withdrawal",
            targets.len()
        ));
        Ok(())
    }

    /// Live pre-tag gate. Chocolatey's community feed is a moderation-queue
    /// one-way door (no programmatic withdraw), so a bad push is expensive and
    /// must be caught BEFORE the tag is cut. Probes every active
    /// `publish.chocolatey` entry: a missing API key, a key the feed rejects,
    /// or a feed it cannot reach all block; a 5xx / ambiguous read warns.
    /// Inactive (`skip:`/`if:`) or unconfigured entries pass without a network
    /// call.
    fn preflight(&self, ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        use crate::publisher_preflight::{FailSeverity, merge};
        use anodizer_core::PreflightCheck;

        // Shallow probe policy: best-effort pre-publish gate, not a write that
        // must land (see `RetryPolicy::PREFLIGHT`).
        let policy = anodizer_core::retry::RetryPolicy::PREFLIGHT;
        // Severity for a DEFINITIVE failure (no key, key rejected, feed
        // unreachable) is the publisher's own required→Blocker / optional→Warning
        // policy — identical to every sibling preflight (gemfury/cloudsmith/…).
        // The community feed being a one-way door does NOT make an OPTIONAL
        // chocolatey entry block the whole release: only a `required:true` entry
        // aborts pre-tag (a transient degrades to Warning below, unless strict
        // preflight promotes it).
        let fail = FailSeverity::for_required(Self::resolved_required(self));
        let mut acc = PreflightCheck::Pass;
        for c in ctx.config.crate_universe() {
            let Some(ch) = c.publish.as_ref().and_then(|p| p.chocolatey.as_ref()) else {
                continue;
            };
            if crate::publisher_helpers::entry_inactive(
                ctx,
                ch.skip.as_ref(),
                None,
                ch.if_condition.as_deref(),
            ) {
                continue;
            }
            // Same source default + URL normalization the push path uses
            // (`publish_to_chocolatey` → `push_nupkg`): probe the exact endpoint
            // the PUT will hit.
            let feed = ch
                .source_repo
                .as_deref()
                .unwrap_or("https://push.chocolatey.org/");
            let api_key = resolve_choco_api_key(ctx, ch);
            if api_key.is_empty() {
                acc = merge(
                    acc,
                    fail.apply(format!(
                        "no chocolatey API key for the push to {feed}; set CHOCOLATEY_API_KEY \
                         or publish.chocolatey.api_key (the community feed is a one-way \
                         moderation queue)"
                    )),
                );
                continue;
            }
            acc = merge(
                acc,
                choco_key_check(
                    &choco_push_url(feed),
                    feed,
                    &api_key,
                    &policy,
                    fail,
                    ctx.preflight_is_strict(),
                    &ctx.logger("preflight"),
                ),
            );
        }
        Ok(acc)
    }
}

/// Per-probe HTTP timeout — long enough for a cold TLS handshake to
/// push.chocolatey.org, short enough that a wedged endpoint cannot stall the
/// pre-tag gate.
const CHOCO_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Resolve the push API key the same way [`super::publish`] does: the
/// template-rendered `api_key` config, else the `CHOCOLATEY_API_KEY` env var.
/// Empty string when neither resolves.
fn resolve_choco_api_key(ctx: &Context, ch: &anodizer_core::config::ChocolateyConfig) -> String {
    ch.api_key
        .as_deref()
        .and_then(|k| ctx.render_template(k).ok())
        .filter(|k| !k.is_empty())
        .or_else(|| ctx.env_var("CHOCOLATEY_API_KEY"))
        .unwrap_or_default()
}

/// Normalize a NuGet V2 source URL to its `…/api/v2/package` push endpoint —
/// the same normalization [`super::package::push_nupkg`] applies so the probe
/// hits exactly the URL the PUT will.
fn choco_push_url(source: &str) -> String {
    let base = source.trim_end_matches('/');
    if base.ends_with("/api/v2/package") {
        base.to_string()
    } else if base.ends_with("/api/v2") {
        format!("{base}/package")
    } else {
        format!("{base}/api/v2/package")
    }
}

/// Verdict of an authenticated probe against the chocolatey push endpoint.
enum ChocoKeyProbe {
    /// 2xx — the feed accepted the `X-NuGet-ApiKey` header.
    Valid,
    /// 401 / 403 — the feed rejected the key.
    Rejected,
    /// Transport failure (DNS / connect / TLS) after the bounded retries —
    /// the feed could not be reached at all.
    Unreachable(String),
    /// 5xx, or an unexpected status (e.g. a feed that disallows GET on the
    /// push route) — reachable, but the verdict is indeterminate.
    Ambiguous(String),
}

/// Map a [`ChocoKeyProbe`] to a [`PreflightCheck`](anodizer_core::PreflightCheck).
/// A DEFINITIVE failure (key rejected, feed unreachable) takes `fail`'s severity
/// — `Blocker` for a `required:true` entry, `Warning` for the default optional
/// config — so a transient DNS/TLS blip on an optional chocolatey never aborts
/// the whole release. An AMBIGUOUS (indeterminate) read warns by default,
/// since a reachable-but-cloudy feed is not proof the key is bad; under strict
/// preflight (`strict`) it is promoted to a blocker (fail-closed).
fn choco_key_check(
    push_url: &str,
    feed: &str,
    api_key: &str,
    policy: &anodizer_core::retry::RetryPolicy,
    fail: crate::publisher_preflight::FailSeverity,
    strict: bool,
    log: &anodizer_core::log::StageLogger,
) -> anodizer_core::PreflightCheck {
    use anodizer_core::PreflightCheck;
    match probe_choco_key(push_url, api_key, policy, log) {
        ChocoKeyProbe::Valid => PreflightCheck::Pass,
        ChocoKeyProbe::Rejected => fail.apply(format!(
            "chocolatey API key rejected by {feed} (HTTP 401/403); the push will fail. \
             Check CHOCOLATEY_API_KEY / publish.chocolatey.api_key"
        )),
        ChocoKeyProbe::Unreachable(reason) => fail.apply(format!(
            "chocolatey feed {feed} unreachable ({reason}); cannot verify the API key before \
             pushing to a one-way moderation queue"
        )),
        ChocoKeyProbe::Ambiguous(reason) => anodizer_core::git::indeterminate_check(
            strict,
            format!(
                "could not verify the chocolatey API key against {feed} ({reason}); \
                 verify CHOCOLATEY_API_KEY manually"
            ),
        ),
    }
}

/// Authenticated GET against the chocolatey push endpoint carrying the
/// `X-NuGet-ApiKey` header (the same header the PUT push uses). 2xx ⇒ key
/// accepted, 401/403 ⇒ rejected, transport failure ⇒ unreachable, anything
/// else ⇒ ambiguous. `push_url` is passed in full so a unit test can point the
/// probe at a local responder without a network round-trip.
///
/// NuGet V2 has no dedicated key-validation endpoint, so this proves the feed
/// is reachable and the key is not outright rejected at the read layer — the
/// strongest pre-push signal obtainable without performing the (one-way) write
/// itself.
fn probe_choco_key(
    push_url: &str,
    api_key: &str,
    policy: &anodizer_core::retry::RetryPolicy,
    log: &anodizer_core::log::StageLogger,
) -> ChocoKeyProbe {
    use anodizer_core::retry::{SuccessClass, http_status, retry_http_blocking};
    let client = match anodizer_core::http::blocking_client(CHOCO_PROBE_TIMEOUT) {
        Ok(c) => c,
        Err(e) => return ChocoKeyProbe::Unreachable(format!("could not build HTTP client: {e}")),
    };
    let key = api_key.to_string();
    let result = retry_http_blocking(
        anodizer_core::retry::RetryLog::new("preflight: chocolatey api key", log),
        policy,
        SuccessClass::Strict,
        |_| {
            client
                .get(push_url)
                .header("X-NuGet-ApiKey", &key)
                .header("Accept", "application/json")
                .send()
        },
        |status, body| {
            format!(
                "{status}: {}",
                anodizer_core::redact::redact_bearer_tokens(body)
            )
        },
    );
    match result {
        Ok(_) => ChocoKeyProbe::Valid,
        Err(err) => match http_status(&err) {
            401 | 403 => ChocoKeyProbe::Rejected,
            0 => ChocoKeyProbe::Unreachable(format!("network failure: {err}")),
            other => ChocoKeyProbe::Ambiguous(format!("unexpected HTTP {other}")),
        },
    }
}

#[cfg(test)]
mod publisher_tests {
    use super::*;
    use anodizer_core::config::{ChocolateyConfig, CrateConfig, PublishConfig, StringOrBool};
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
    use anodizer_core::{PreflightCheck, PublishEvidence, Publisher, PublisherGroup};

    /// A crate carrying a `publish.chocolatey` block whose `source_repo` points
    /// the preflight probe at `source` (a local responder in tests).
    fn choco_crate_src(name: &str, source: &str) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            publish: Some(PublishConfig {
                chocolatey: Some(ChocolateyConfig {
                    source_repo: Some(source.to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn http(status_line: &str, body: &str) -> &'static str {
        Box::leak(
            format!(
                "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        )
    }

    fn choco_crate(crate_name: &str, package_name: Option<&str>) -> CrateConfig {
        CrateConfig {
            name: crate_name.to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            publish: Some(PublishConfig {
                chocolatey: Some(ChocolateyConfig {
                    name: package_name.map(|s| s.to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn chocolatey_publisher_classification() {
        let p = ChocolateyPublisher::new();
        assert_eq!(p.name(), "chocolatey");
        assert_eq!(p.group(), PublisherGroup::Submitter);
        assert!(!p.required());
        assert_eq!(p.rollback_scope_needed(), None);
    }

    #[test]
    fn config_fully_inactive_true_when_selected_crate_is_skipped_sibling_active() {
        let mut skipped = choco_crate("x", None);
        skipped
            .publish
            .as_mut()
            .unwrap()
            .chocolatey
            .as_mut()
            .unwrap()
            .skip = Some(StringOrBool::Bool(true));
        let ctx = TestContextBuilder::new()
            .crates(vec![skipped, choco_crate("y", None)])
            .selected_crates(vec!["x".to_string()])
            .build();
        assert!(
            ChocolateyPublisher::new().config_fully_inactive(&ctx),
            "selecting a skipped crate must not be masked as active by an out-of-scope sibling"
        );
    }

    /// Empty `--crate` selection means "all crates" — an active entry with
    /// no `--crate` filter applied must keep the publisher live.
    #[test]
    fn config_fully_inactive_false_with_empty_selection_and_active_entry() {
        let ctx = TestContextBuilder::new()
            .crates(vec![choco_crate("x", None)])
            .build();

        assert!(
            !ChocolateyPublisher::new().config_fully_inactive(&ctx),
            "empty selection means \"all crates\"; an active entry must keep the \
             publisher live"
        );
    }

    #[test]
    fn chocolatey_preflight_defaults_to_pass() {
        let ctx = TestContextBuilder::new().build();
        let p = ChocolateyPublisher::new();
        assert!(matches!(
            p.preflight(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn chocolatey_preflight_passes_when_key_accepted() {
        // 2xx from the push endpoint with the key header ⇒ Pass.
        let (addr, _c) = spawn_oneshot_http_responder(vec![http("200 OK", "")]);
        let ctx = TestContextBuilder::new()
            .crates(vec![choco_crate_src("demo", &format!("http://{addr}/"))])
            .env("CHOCOLATEY_API_KEY", "good-key")
            .build();
        let p = ChocolateyPublisher::new();
        assert!(matches!(
            p.preflight(&ctx).expect("preflight"),
            PreflightCheck::Pass
        ));
    }

    /// The DEFAULT chocolatey config is OPTIONAL (`required()==false`). A
    /// definitive 403 (key rejected) must therefore WARN, not Blocker — an
    /// optional surface never aborts the whole release pre-tag. The severity now
    /// routes through `FailSeverity::for_required` like every sibling publisher.
    #[test]
    fn chocolatey_preflight_optional_warns_when_key_rejected() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![http("403 Forbidden", "")]);
        let ctx = TestContextBuilder::new()
            .crates(vec![choco_crate_src("demo", &format!("http://{addr}/"))])
            .env("CHOCOLATEY_API_KEY", "bad-key")
            .build();
        let p = ChocolateyPublisher::new();
        match p.preflight(&ctx).expect("preflight") {
            PreflightCheck::Warning(m) => assert!(m.contains("rejected"), "{m}"),
            other => panic!("expected Warning on a 403 for an optional choco entry, got {other:?}"),
        }
    }

    /// When the operator sets `chocolatey.required: true`, a rejected key MUST
    /// Blocker — the genuine one-way-door-must-not-fire-on-bad-key case the
    /// docstring defends. Same probe, severity flipped by the required override.
    #[test]
    fn chocolatey_preflight_required_blocks_when_key_rejected() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![http("403 Forbidden", "")]);
        let ctx = TestContextBuilder::new()
            .crates(vec![choco_crate_src("demo", &format!("http://{addr}/"))])
            .env("CHOCOLATEY_API_KEY", "bad-key")
            .build();
        let p = ChocolateyPublisher::with_overrides(Some(true), None);
        match p.preflight(&ctx).expect("preflight") {
            PreflightCheck::Blocker(m) => assert!(m.contains("rejected"), "{m}"),
            other => panic!("expected Blocker on a 403 for a required choco entry, got {other:?}"),
        }
    }

    /// An OPTIONAL choco entry with no resolvable key ⇒ Warning (the push would
    /// no-op/skip; do not abort the release for an optional surface).
    #[test]
    fn chocolatey_preflight_optional_warns_when_key_missing() {
        let ctx = TestContextBuilder::new()
            .crates(vec![choco_crate_src(
                "demo",
                "https://push.chocolatey.org/",
            )])
            .sealed_env()
            .build();
        let p = ChocolateyPublisher::new();
        match p.preflight(&ctx).expect("preflight") {
            PreflightCheck::Warning(m) => assert!(m.contains("no chocolatey API key"), "{m}"),
            other => panic!("expected Warning when an optional choco key is absent, got {other:?}"),
        }
    }

    /// A REQUIRED choco entry with no resolvable key ⇒ Blocker, with no network
    /// call — the operator asked for chocolatey, so a missing key must abort.
    #[test]
    fn chocolatey_preflight_required_blocks_when_key_missing() {
        let ctx = TestContextBuilder::new()
            .crates(vec![choco_crate_src(
                "demo",
                "https://push.chocolatey.org/",
            )])
            .sealed_env()
            .build();
        let p = ChocolateyPublisher::with_overrides(Some(true), None);
        match p.preflight(&ctx).expect("preflight") {
            PreflightCheck::Blocker(m) => assert!(m.contains("no chocolatey API key"), "{m}"),
            other => panic!("expected Blocker when a required choco key is absent, got {other:?}"),
        }
    }

    /// The exact bug: a transient UNREACHABLE feed (closed port / DNS blip) on
    /// the DEFAULT optional config must WARN, never abort the whole release. The
    /// old hardcoded Blocker turned every feed-side hiccup into a release-killer.
    #[test]
    fn chocolatey_preflight_optional_warns_when_feed_unreachable() {
        let ctx = TestContextBuilder::new()
            .crates(vec![choco_crate_src("demo", "http://127.0.0.1:1/")])
            .env("CHOCOLATEY_API_KEY", "some-key")
            .build();
        let p = ChocolateyPublisher::new();
        match p.preflight(&ctx).expect("preflight") {
            PreflightCheck::Warning(m) => assert!(m.contains("unreachable"), "{m}"),
            other => {
                panic!("expected Warning on an unreachable optional feed, got {other:?}")
            }
        }
    }

    /// A REQUIRED entry whose feed is unreachable still Blocks — the key cannot
    /// be verified before pushing to a one-way moderation queue.
    #[test]
    fn chocolatey_preflight_required_blocks_when_feed_unreachable() {
        let ctx = TestContextBuilder::new()
            .crates(vec![choco_crate_src("demo", "http://127.0.0.1:1/")])
            .env("CHOCOLATEY_API_KEY", "some-key")
            .build();
        let p = ChocolateyPublisher::with_overrides(Some(true), None);
        match p.preflight(&ctx).expect("preflight") {
            PreflightCheck::Blocker(m) => assert!(m.contains("unreachable"), "{m}"),
            other => panic!("expected Blocker on an unreachable required feed, got {other:?}"),
        }
    }

    #[test]
    fn chocolatey_preflight_passes_when_skip_truthy() {
        // skip:true ⇒ inactive entry ⇒ Pass with no probe (the source points at
        // a closed port that would otherwise surface as unreachable).
        let mut crate_cfg = choco_crate_src("demo", "http://127.0.0.1:1/");
        if let Some(ch) = crate_cfg
            .publish
            .as_mut()
            .and_then(|p| p.chocolatey.as_mut())
        {
            ch.skip = Some(StringOrBool::Bool(true));
        }
        let ctx = TestContextBuilder::new()
            .crates(vec![crate_cfg])
            .env("CHOCOLATEY_API_KEY", "good-key")
            .build();
        let p = ChocolateyPublisher::new();
        assert!(matches!(
            p.preflight(&ctx).expect("preflight"),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn chocolatey_requirements_emit_xmllint_tool() {
        // The strict pre-publish gate schema-validates the rendered nuspec via
        // `xmllint --schema` and HARD-FAILS when the tool is absent (moderation
        // submission is a one-way door), so requirements() must report it —
        // otherwise the action's auto-install (driven by `anodizer tools`)
        // leaves it off a clean runner and the release dies at the prepublish
        // guard after the GitHub release already shipped.
        let ctx = TestContextBuilder::new()
            .crates(vec![choco_crate("demo", None)])
            .build();
        let reqs = ChocolateyPublisher::new().requirements(&ctx);
        assert!(
            reqs.iter().any(|r| matches!(
                r,
                anodizer_core::EnvRequirement::Tool { name } if name == "xmllint"
            )),
            "expected a mandatory Tool{{name:\"xmllint\"}} requirement; got: {reqs:?}"
        );
    }

    #[test]
    fn chocolatey_requirements_omit_xmllint_when_all_entries_skipped() {
        // Every entry inactive ⇒ the publisher renders/validates nothing, so
        // demanding xmllint would gate a run that never touches a nuspec.
        let mut crate_cfg = choco_crate("demo", None);
        if let Some(ch) = crate_cfg
            .publish
            .as_mut()
            .and_then(|p| p.chocolatey.as_mut())
        {
            ch.skip = Some(StringOrBool::Bool(true));
        }
        let ctx = TestContextBuilder::new().crates(vec![crate_cfg]).build();
        let reqs = ChocolateyPublisher::new().requirements(&ctx);
        assert!(
            !reqs.iter().any(|r| matches!(
                r,
                anodizer_core::EnvRequirement::Tool { name } if name == "xmllint"
            )),
            "expected NO xmllint Tool requirement when every entry is skipped; got: {reqs:?}"
        );
    }

    #[test]
    fn chocolatey_rollback_warns_when_no_targets_recorded() {
        let capture = anodizer_core::log::LogCapture::new();
        let mut ctx = TestContextBuilder::new().build();
        ctx.with_log_capture(capture.clone());
        let evidence = PublishEvidence::new("chocolatey");
        let p = ChocolateyPublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());

        let warns = capture.warn_messages();
        assert!(
            warns.iter().any(|m| m.contains("chocolatey")
                && m.contains("submitted packages")
                && m.contains("verify")),
            "expected captured warn naming publisher + target-noun + 'verify'; got: {warns:?}"
        );
    }

    #[test]
    fn chocolatey_rollback_warns_per_target_when_evidence_present() {
        // Warn-only when targets are recorded; assert it does NOT
        // return Err so the dispatch chain continues.
        let mut ctx = TestContextBuilder::new().build();
        let mut evidence = PublishEvidence::new("chocolatey");
        evidence.extra = anodizer_core::PublishEvidenceExtra::Chocolatey(
            anodizer_core::publish_evidence::ChocolateyExtra {
                chocolatey_targets: vec![
                    ChocolateyTarget {
                        target: "demo".into(),
                        crate_name: "demo".into(),
                        package_id: "demo".into(),
                        version: "1.2.3".into(),
                    },
                    ChocolateyTarget {
                        target: "widget".into(),
                        crate_name: "widget".into(),
                        package_id: "widget".into(),
                        version: "1.2.3".into(),
                    },
                ],
            },
        );
        let p = ChocolateyPublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());
        assert_eq!(decode_chocolatey_targets(&evidence.extra).len(), 2);
    }

    #[test]
    fn chocolatey_target_extra_roundtrips() {
        let original = vec![ChocolateyTarget {
            target: "demo".into(),
            crate_name: "demo".into(),
            package_id: "demo".into(),
            version: "1.2.3".into(),
        }];
        let extra = anodizer_core::PublishEvidenceExtra::Chocolatey(
            anodizer_core::publish_evidence::ChocolateyExtra {
                chocolatey_targets: original.clone(),
            },
        );
        let decoded = decode_chocolatey_targets(&extra);
        assert_eq!(decoded, original);
    }

    #[test]
    fn chocolatey_target_extra_carries_no_secret_material() {
        // Structural pin: build a typed-variant evidence and assert
        // (a) no credential-shaped keys appear AND (b) the
        // operator-public gallery coordinates are preserved.
        let mut e = PublishEvidence::new("chocolatey");
        e.extra = anodizer_core::PublishEvidenceExtra::Chocolatey(
            anodizer_core::publish_evidence::ChocolateyExtra {
                chocolatey_targets: vec![ChocolateyTarget {
                    target: "demo".into(),
                    crate_name: "demo".into(),
                    package_id: "demo".into(),
                    version: "1.2.3".into(),
                }],
            },
        );
        let s = serde_json::to_string(&e).expect("serialize");
        assert!(!s.contains("\"token\":"), "{s}");
        assert!(!s.contains("\"api_key\":"), "{s}");
        assert!(!s.contains("\"apikey\":"), "{s}");
        assert!(!s.contains("\"auth\":"), "{s}");
        assert!(!s.contains("\"password\":"), "{s}");
        assert!(!s.contains("\"secret\":"), "{s}");
        // Positive shape: gallery coordinates present.
        assert!(s.contains("\"package_id\":\"demo\""), "{s}");
        assert!(s.contains("\"version\":\"1.2.3\""), "{s}");
    }

    #[test]
    fn chocolatey_collect_target_resolves_package_name_override() {
        let ctx = TestContextBuilder::new()
            .crates(vec![choco_crate("demo", Some("DemoTool"))])
            .build();
        let t = collect_chocolatey_target(&ctx, "demo").expect("target");
        assert_eq!(t.crate_name, "demo");
        assert_eq!(t.package_id, "DemoTool");
    }

    #[test]
    fn chocolatey_collect_target_defaults_to_crate_name() {
        let ctx = TestContextBuilder::new()
            .crates(vec![choco_crate("demo", None)])
            .build();
        let t = collect_chocolatey_target(&ctx, "demo").expect("target");
        assert_eq!(t.package_id, "demo");
    }

    /// A workspace-only crate (pure-workspace config) must snapshot a
    /// rollback target: the nupkg enters the moderation queue (a one-way
    /// door), so a `None` here means no manual-withdrawal pointer and no
    /// `primary_ref` for a package that WAS submitted.
    #[test]
    fn chocolatey_collect_target_sees_workspace_only_crate() {
        let ctx = TestContextBuilder::new()
            .workspaces(vec![anodizer_core::config::WorkspaceConfig {
                name: "ws".to_string(),
                crates: vec![choco_crate("ws-only", Some("WsTool"))],
                ..Default::default()
            }])
            .build();
        assert!(
            ctx.config.crates.is_empty(),
            "fixture must be a pure-workspace config"
        );
        let t = collect_chocolatey_target(&ctx, "ws-only").expect("target snapshot");
        assert_eq!(t.crate_name, "ws-only");
        assert_eq!(t.package_id, "WsTool");
    }

    // Log-message helpers — the operator-facing log strings the publisher
    // emits at each boundary. The failure mode these guard against: a
    // publisher whose iteration loop hits only silently-`continue`d
    // crates returns Ok with an empty evidence record, which the
    // dispatch table then reports as "succeeded" — indistinguishable
    // from a real push. Every helper below must produce a line the
    // operator can grep the publish log for.

    #[test]
    fn run_per_crate_start_message_names_crate() {
        let msg = run_per_crate_start_message("demo");
        assert!(
            msg.starts_with("starting per-crate chocolatey publish"),
            "{msg}"
        );
        assert!(msg.contains("'demo'"), "{msg}");
    }

    #[test]
    fn run_done_message_reports_processed_count() {
        let msg = run_done_message(2);
        assert!(msg.starts_with("finished chocolatey publish"), "{msg}");
        assert!(msg.contains("2 configured crate(s) processed"), "{msg}");
    }

    #[test]
    fn run_no_eligible_crates_warning_names_remediation() {
        let msg = run_no_eligible_crates_warning(5);
        assert!(msg.starts_with("chocolatey publisher registered"), "{msg}");
        assert!(msg.contains("0 of 5 effective"), "{msg}");
        assert!(msg.contains("nothing pushed"), "{msg}");
        // The warning must point the operator at the remediation surface
        // (--crate / --all selection) — otherwise it's noise.
        assert!(msg.contains("--crate"), "{msg}");
        assert!(msg.contains("--all"), "{msg}");
    }

    #[test]
    fn run_no_eligible_crates_warning_handles_empty_selection() {
        // The zero-effective case (no crate carries a `publish.chocolatey`
        // block) must produce the remediation string with a 0/0 count.
        // The warn helper must not panic or omit the remediation text in
        // this shape.
        let msg = run_no_eligible_crates_warning(0);
        assert!(msg.starts_with("chocolatey publisher registered"), "{msg}");
        assert!(msg.contains("0 of 0 effective"), "{msg}");
        assert!(msg.contains("nothing pushed"), "{msg}");
        assert!(msg.contains("--crate"), "{msg}");
        assert!(msg.contains("--all"), "{msg}");
    }

    /// Run the publisher end-to-end in dry-run mode against a context
    /// that selects a choco-configured crate. Verifies the run path
    /// executes the configured crate (returns Ok with the "chocolatey"
    /// evidence name) but does NOT record rollback targets — dry-run
    /// pushes nothing, so recording a target would later mislead
    /// rollback into emitting a "manual withdrawal required" warning
    /// for a package this run never submitted.
    #[test]
    fn chocolatey_publisher_run_dry_run_executes_without_recording_targets() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        let repo = crate::testing::hermetic_tagged_repo();
        let mut ctx = TestContextBuilder::new()
            .crates(vec![choco_crate("demo", None)])
            .selected_crates(vec!["demo".to_string()])
            .dry_run(true)
            .project_root(repo.path().to_path_buf())
            .build();
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from("/tmp/demo-windows-amd64.zip"),
            name: "demo-windows-amd64.zip".to_string(),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "demo".to_string(),
            metadata: {
                let mut m = std::collections::HashMap::new();
                m.insert("sha256".to_string(), "deadbeef".to_string());
                m.insert("url".to_string(), "https://example.com/x.zip".to_string());
                m
            },
            size: None,
        });
        let p = ChocolateyPublisher::new();
        let evidence = p.run(&mut ctx).expect("dry-run publisher.run");
        assert_eq!(evidence.publisher, "chocolatey");
        assert!(
            evidence.primary_ref.is_none(),
            "dry-run must not record a primary_ref — nothing was pushed; \
             primary_ref={:?}",
            evidence.primary_ref
        );
        let targets = decode_chocolatey_targets(&evidence.extra);
        assert!(
            targets.is_empty(),
            "dry-run must not record rollback targets; got {:?}",
            targets
        );
    }

    /// When the publisher is registered (a crate has a choco block) but
    /// the selected-crates filter excludes every choco-configured
    /// crate, the run path must still return Ok (so the dispatch chain
    /// doesn't abort), but record no targets — and the operator-facing
    /// warning helper must produce a remediation-pointing string.
    #[test]
    fn chocolatey_publisher_run_no_eligible_crates_returns_empty_evidence() {
        let mut ctx = TestContextBuilder::new()
            .crates(vec![
                choco_crate("demo", None),
                CrateConfig {
                    name: "other".to_string(),
                    path: ".".to_string(),
                    tag_template: Some("v{{ .Version }}".to_string()),
                    publish: Some(PublishConfig::default()),
                    ..Default::default()
                },
            ])
            // Select only the non-choco crate — the publisher should
            // still be registered (because `demo` has a block) but its
            // run path will iterate zero choco-configured crates.
            .selected_crates(vec!["other".to_string()])
            .dry_run(true)
            .build();
        let p = ChocolateyPublisher::new();
        let evidence = p.run(&mut ctx).expect("publisher.run ok");
        assert!(
            evidence.primary_ref.is_none(),
            "no choco-eligible crate selected, primary_ref must be unset"
        );
        let targets = decode_chocolatey_targets(&evidence.extra);
        assert!(
            targets.is_empty(),
            "no choco-eligible crate selected, targets must be empty"
        );
    }

    /// Default-empty `selected_crates` (the `ContextOptions::default()`
    /// shape, produced by `release --publish-only` with no
    /// `--crate`/`--all`) MUST resolve to implicit-all over every crate
    /// carrying a `publish.chocolatey` block. Without this the publisher
    /// would emit `run_done_message(0)` and silently report success.
    ///
    /// Asserted via the non-dry-run path: in dry-run, target snapshots
    /// aren't recorded (push didn't happen), so the most direct probe
    /// of "loop body executed for demo" is to call
    /// `effective_publish_crates` with the same predicate the run loop
    /// uses. A regression that breaks implicit-all returns an empty
    /// list here.
    #[test]
    fn chocolatey_publisher_run_empty_selection_includes_all_configured() {
        let ctx = TestContextBuilder::new()
            .crates(vec![choco_crate("demo", None)])
            // selected_crates intentionally left at the default Vec::new()
            .dry_run(true)
            .build();
        let names = crate::publisher_helpers::effective_publish_crates(
            &ctx,
            is_chocolatey_per_crate_configured,
        );
        assert_eq!(
            names,
            vec!["demo".to_string()],
            "empty selection must implicitly include every choco-configured crate"
        );
    }

    /// Implicit-all must still produce empty evidence when zero crates
    /// carry a `publish.chocolatey` block — the warn helper fires on
    /// "registered but nothing eligible", which is meaningful only when
    /// no crate is configured at all.
    #[test]
    fn chocolatey_publisher_run_empty_selection_with_no_configured_crate_returns_empty_evidence() {
        let mut ctx = TestContextBuilder::new()
            .crates(vec![CrateConfig {
                name: "other".to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                publish: Some(PublishConfig::default()),
                ..Default::default()
            }])
            .dry_run(true)
            .build();
        let p = ChocolateyPublisher::new();
        let evidence = p.run(&mut ctx).expect("publisher.run ok");
        assert!(
            evidence.primary_ref.is_none(),
            "no choco-configured crate present, primary_ref must be unset"
        );
        let targets = decode_chocolatey_targets(&evidence.extra);
        assert!(
            targets.is_empty(),
            "no choco-configured crate present, targets must be empty"
        );
    }

    #[test]
    fn chocolatey_publisher_visible_work_contract() {
        use crate::testing::assert_publisher_visible_work_contract;
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        let repo = crate::testing::hermetic_tagged_repo();
        let mut ctx = TestContextBuilder::new()
            .crates(vec![choco_crate("demo", None)])
            .selected_crates(vec!["demo".to_string()])
            .dry_run(true)
            .project_root(repo.path().to_path_buf())
            .build();
        // Chocolatey's publish path resolves a Windows archive artifact — without
        // one configured here the per-crate publish would bail before emitting
        // the per-crate-start status line. Mirror the chocolatey dry-run test
        // setup so the loop actually executes the visible-work sequence.
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from("/tmp/demo-windows-amd64.zip"),
            name: "demo-windows-amd64.zip".to_string(),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "demo".to_string(),
            metadata: {
                let mut m = std::collections::HashMap::new();
                m.insert("sha256".to_string(), "deadbeef".to_string());
                m.insert("url".to_string(), "https://example.com/x.zip".to_string());
                m
            },
            size: None,
        });
        let p = ChocolateyPublisher::new();
        assert_publisher_visible_work_contract(&p, &mut ctx);
    }
}
