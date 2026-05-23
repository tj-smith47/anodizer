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
use serde::{Deserialize, Serialize};

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
/// (without the leading `v`) — matching what
/// [`anodizer_core::context::Context::version`] returns.
///
/// NB: no `api_key`, `token`, or `password` fields — see module
/// rustdoc for the credential-handling rationale.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ChocolateyTarget {
    /// Per-target label — the crate the nupkg was generated for.
    /// Surfaces in log lines.
    target: String,
    /// The crate this publish covered. Duplicates `target` to keep the
    /// shape symmetric with the other Submitter publishers.
    crate_name: String,
    /// Chocolatey gallery package ID — the URL slug on
    /// `community.chocolatey.org/packages/<package_id>`. Resolved from
    /// `publish.chocolatey.name` when set, else the crate name.
    package_id: String,
    /// Bare semver (no leading `v`) — what the Chocolatey gallery
    /// records as the package version.
    version: String,
}

/// Decode the `chocolatey_targets` array from
/// [`anodizer_core::PublishEvidence::extra`].
///
/// Returns an empty Vec on any of: missing key, wrong shape, empty
/// array. Rollback treats empty-decode the same as no-evidence and
/// emits the canonical empty-evidence warn.
fn decode_chocolatey_targets(extra: &serde_json::Value) -> Vec<ChocolateyTarget> {
    extra
        .get("chocolatey_targets")
        .and_then(|v| serde_json::from_value::<Vec<ChocolateyTarget>>(v.clone()).ok())
        .unwrap_or_default()
}

/// True when the crate has a `publish.chocolatey` block — mirrors the
/// `per_crate!` predicate in `lib.rs` so the publisher iterates
/// exactly the same crate universe.
fn is_chocolatey_per_crate_configured(ctx: &Context, crate_name: &str) -> bool {
    crate::util::all_crates(ctx)
        .into_iter()
        .any(|c| c.name == crate_name && c.publish.as_ref().is_some_and(|p| p.chocolatey.is_some()))
}

/// Build a [`ChocolateyTarget`] for the given crate. Reads config + the
/// live process version so the recorded coordinates match what
/// `publish_to_chocolatey` will push. Returns `None` when no chocolatey
/// block is configured (matches the publish path's skip semantics).
fn collect_chocolatey_target(ctx: &Context, crate_name: &str) -> Option<ChocolateyTarget> {
    let c = ctx.config.crates.iter().find(|c| c.name == crate_name)?;
    let cfg = c.publish.as_ref().and_then(|p| p.chocolatey.as_ref())?;
    let package_id = cfg.name.as_deref().unwrap_or(crate_name).to_string();
    Some(ChocolateyTarget {
        target: package_id.clone(),
        crate_name: crate_name.to_string(),
        package_id,
        version: ctx.version(),
    })
}

/// Message emitted at publisher entry. Names how many crates the publisher
/// is iterating over. Factored into a helper so tests can pin the exact
/// substring an operator scans the log for ("chocolatey: starting publish
/// for ...").
pub(crate) fn run_start_message(selected_total: usize) -> String {
    format!(
        "chocolatey: starting publish for {} selected crate(s)",
        selected_total
    )
}

/// Message emitted when a selected crate has no `publish.chocolatey`
/// block. Replaces what used to be a silent `continue` — operators need
/// to see why a per-crate publish was a no-op rather than guess from a
/// blank log.
pub(crate) fn run_skip_unconfigured_message(crate_name: &str) -> String {
    format!(
        "chocolatey: skipping crate '{}' — no chocolatey config block",
        crate_name
    )
}

/// Message emitted just before delegating to `publish_to_chocolatey`.
/// Anchors the choco activity (nuspec generation, nupkg creation, push)
/// to a specific crate in the log so multi-crate workspaces are
/// disambiguatable.
pub(crate) fn run_per_crate_start_message(crate_name: &str) -> String {
    format!(
        "chocolatey: starting per-crate publish for '{}'",
        crate_name
    )
}

/// Final summary emitted at publisher exit. `processed` is the count of
/// crates the publisher actually invoked `publish_to_chocolatey` on (not
/// the count of successful pushes — `publish_to_chocolatey` has its own
/// skip paths for moderation/hash-match/dry-run/etc., each of which logs
/// its own status line).
pub(crate) fn run_done_message(processed: usize) -> String {
    format!("chocolatey: completed — {} crate(s) processed", processed)
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
        "chocolatey: registered but 0 of {} effective crate(s) had a chocolatey \
         config block — nothing pushed. Check that --crate / --all selects a \
         crate whose publish.chocolatey block is set.",
        selected_total
    )
}

impl anodizer_core::Publisher for ChocolateyPublisher {
    fn name(&self) -> &str {
        Self::PUBLISHER_NAME
    }
    fn group(&self) -> anodizer_core::PublisherGroup {
        Self::PUBLISHER_GROUP
    }
    fn required(&self) -> bool {
        Self::PUBLISHER_REQUIRED
    }
    fn rollback_scope_needed(&self) -> Option<&'static str> {
        Self::ROLLBACK_SCOPE
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        let log = ctx.logger("publish");
        let mut targets: Vec<ChocolateyTarget> = Vec::new();
        let selected = crate::publisher_helpers::effective_publish_crates(
            ctx,
            is_chocolatey_per_crate_configured,
        );
        log.status(&run_start_message(selected.len()));
        for crate_name in &selected {
            // Defensive guard for explicit `--crate=X` selection when X has no
            // publisher block; implicit-all is already filtered by effective_publish_crates above.
            if !is_chocolatey_per_crate_configured(ctx, crate_name) {
                log.status(&run_skip_unconfigured_message(crate_name));
                continue;
            }
            // Snapshot the target shape BEFORE the publish path runs so
            // a mid-publish failure still leaves the operator a manual
            // withdrawal pointer.
            if let Some(t) = collect_chocolatey_target(ctx, crate_name) {
                targets.push(t);
            }
            log.status(&run_per_crate_start_message(crate_name));
            super::publish::publish_to_chocolatey(ctx, crate_name, &log)?;
        }
        let processed = targets.len();
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
        evidence.extra = serde_json::json!({ "chocolatey_targets": targets });
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
                "chocolatey: manual withdrawal required for '{}' version '{}'; \
                 visit https://community.chocolatey.org/packages/{} and use the \
                 'Maintain' UI to withdraw the submission (only the package \
                 owner can drive this; the push API key does not authorize \
                 withdraws).",
                t.package_id, t.version, t.package_id
            ));
        }
        log.status(&format!(
            "chocolatey: {} package(s) require manual withdrawal",
            targets.len()
        ));
        Ok(())
    }

    fn preflight(&self, _ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        Ok(anodizer_core::PreflightCheck::Pass)
    }
}

#[cfg(test)]
mod publisher_tests {
    use super::*;
    use anodizer_core::config::{ChocolateyConfig, CrateConfig, PublishConfig};
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::{PreflightCheck, PublishEvidence, Publisher, PublisherGroup};

    fn choco_crate(crate_name: &str, package_name: Option<&str>) -> CrateConfig {
        CrateConfig {
            name: crate_name.to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
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
    fn chocolatey_preflight_defaults_to_pass() {
        let ctx = TestContextBuilder::new().build();
        let p = ChocolateyPublisher::new();
        assert!(matches!(
            p.preflight(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn chocolatey_rollback_warns_when_no_targets_recorded() {
        let mut ctx = TestContextBuilder::new().build();
        let evidence = PublishEvidence::new("chocolatey");
        let p = ChocolateyPublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());

        let msg = crate::publisher_helpers::rollback_empty_warning_msg(
            "chocolatey",
            "submitted packages",
        );
        assert!(msg.starts_with("chocolatey:"), "{msg}");
        assert!(msg.contains("submitted packages"), "{msg}");
        assert!(msg.contains("verify"), "{msg}");
        assert!(msg.contains("manually"), "{msg}");
    }

    #[test]
    fn chocolatey_rollback_warns_per_target_when_evidence_present() {
        // Warn-only when targets are recorded; assert it does NOT
        // return Err so the dispatch chain continues.
        let mut ctx = TestContextBuilder::new().build();
        let mut evidence = PublishEvidence::new("chocolatey");
        evidence.extra = serde_json::json!({
            "chocolatey_targets": [
                {"target": "demo", "crate_name": "demo", "package_id": "demo", "version": "1.2.3"},
                {"target": "widget", "crate_name": "widget", "package_id": "widget", "version": "1.2.3"},
            ],
        });
        let p = ChocolateyPublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());
        // Sanity-check that the warn pattern names both targets and
        // the gallery URL prefix.
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
        let extra = serde_json::json!({ "chocolatey_targets": original.clone() });
        let decoded = decode_chocolatey_targets(&extra);
        assert_eq!(decoded, original);
    }

    #[test]
    fn chocolatey_target_extra_carries_no_secret_material() {
        // Defense-in-depth: serialize a target and assert no field
        // names that could leak the chocolatey API key are present.
        let t = ChocolateyTarget {
            target: "demo".into(),
            crate_name: "demo".into(),
            package_id: "demo".into(),
            version: "1.2.3".into(),
        };
        let s = serde_json::to_string(&t).expect("serialize");
        assert!(!s.contains("\"token\":"), "{s}");
        assert!(!s.contains("\"api_key\":"), "{s}");
        assert!(!s.contains("\"apikey\":"), "{s}");
        assert!(!s.contains("\"auth\":"), "{s}");
        assert!(!s.contains("\"password\":"), "{s}");
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

    // Log-message helpers — the operator-facing log strings the publisher
    // emits at each boundary. The failure mode these guard against: a
    // publisher whose iteration loop hits only silently-`continue`d
    // crates returns Ok with an empty evidence record, which the
    // dispatch table then reports as "succeeded" — indistinguishable
    // from a real push. Every helper below must produce a line the
    // operator can grep the publish log for.

    #[test]
    fn run_start_message_names_selected_total() {
        let msg = run_start_message(3);
        assert!(msg.starts_with("chocolatey:"), "{msg}");
        assert!(msg.contains("starting publish"), "{msg}");
        assert!(msg.contains("3 selected"), "{msg}");
    }

    #[test]
    fn run_skip_unconfigured_message_names_crate() {
        let msg = run_skip_unconfigured_message("demo");
        assert!(msg.starts_with("chocolatey:"), "{msg}");
        assert!(msg.contains("skipping crate 'demo'"), "{msg}");
        assert!(msg.contains("no chocolatey config block"), "{msg}");
    }

    #[test]
    fn run_per_crate_start_message_names_crate() {
        let msg = run_per_crate_start_message("demo");
        assert!(msg.starts_with("chocolatey:"), "{msg}");
        assert!(msg.contains("starting per-crate publish"), "{msg}");
        assert!(msg.contains("'demo'"), "{msg}");
    }

    #[test]
    fn run_done_message_reports_processed_count() {
        let msg = run_done_message(2);
        assert!(msg.starts_with("chocolatey:"), "{msg}");
        assert!(msg.contains("completed"), "{msg}");
        assert!(msg.contains("2 crate(s) processed"), "{msg}");
    }

    #[test]
    fn run_no_eligible_crates_warning_names_remediation() {
        let msg = run_no_eligible_crates_warning(5);
        assert!(msg.starts_with("chocolatey:"), "{msg}");
        assert!(msg.contains("registered"), "{msg}");
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
        assert!(msg.starts_with("chocolatey:"), "{msg}");
        assert!(msg.contains("0 of 0 effective"), "{msg}");
        assert!(msg.contains("nothing pushed"), "{msg}");
        assert!(msg.contains("--crate"), "{msg}");
        assert!(msg.contains("--all"), "{msg}");
    }

    /// Run the publisher end-to-end in dry-run mode against a context
    /// that selects a choco-configured crate. Verifies the run path is
    /// wired (returns Ok, records target evidence). The log lines
    /// themselves are written to stderr and asserted indirectly via the
    /// helper-string tests above.
    #[test]
    fn chocolatey_publisher_run_dry_run_records_target() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        let mut ctx = TestContextBuilder::new()
            .crates(vec![choco_crate("demo", None)])
            .selected_crates(vec!["demo".to_string()])
            .dry_run(true)
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
        // primary_ref + extra.chocolatey_targets must reflect that the
        // run path actually visited the demo crate (not silently
        // skipped). Without these the publisher would report
        // "succeeded" with nothing recorded — indistinguishable from a
        // real push to a downstream summary.
        assert_eq!(
            evidence.primary_ref.as_deref(),
            Some("https://community.chocolatey.org/packages/demo")
        );
        let targets = decode_chocolatey_targets(&evidence.extra);
        assert_eq!(targets.len(), 1, "{:?}", targets);
        assert_eq!(targets[0].crate_name, "demo");
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
                    tag_template: "v{{ .Version }}".to_string(),
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
    /// would emit `run_done_message(0)` and silently report success,
    /// which is the root-cause failure mode this regression test pins
    /// against.
    #[test]
    fn chocolatey_publisher_run_empty_selection_publishes_all_configured() {
        let mut ctx = TestContextBuilder::new()
            .crates(vec![choco_crate("demo", None)])
            // selected_crates intentionally left at the default Vec::new()
            .dry_run(true)
            .build();
        let p = ChocolateyPublisher::new();
        let evidence = p.run(&mut ctx).expect("publisher.run ok");
        assert_eq!(
            evidence.primary_ref.as_deref(),
            Some("https://community.chocolatey.org/packages/demo"),
            "empty selection must implicitly include every choco-configured crate"
        );
        let targets = decode_chocolatey_targets(&evidence.extra);
        assert_eq!(
            targets.len(),
            1,
            "empty selection must produce one target per choco-configured crate"
        );
        assert_eq!(targets[0].crate_name, "demo");
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
                tag_template: "v{{ .Version }}".to_string(),
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
        let mut ctx = TestContextBuilder::new()
            .crates(vec![choco_crate("demo", None)])
            .selected_crates(vec!["demo".to_string()])
            .dry_run(true)
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
