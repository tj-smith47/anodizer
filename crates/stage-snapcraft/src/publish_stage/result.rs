use anodizer_core::context::Context;
use anodizer_core::{
    PublishEvidence, PublishReport, PublisherGroup, PublisherOutcome, PublisherResult,
};

use crate::targets::SnapcraftTarget;

// ---------------------------------------------------------------------------
// PublisherResult recording
// ---------------------------------------------------------------------------

/// Build the `PublishEvidence` recorded on a successful snapcraft run.
///
/// `primary_ref` points at the first uploaded package's snapcraft.io
/// listing; `extra.snapcraft_targets` carries the full per-target
/// snapshot used by `--rollback-only --from-run` to surface the
/// (package, channel) tuples an operator needs to address manually.
pub(crate) fn build_snapcraft_evidence(targets: &[SnapcraftTarget]) -> PublishEvidence {
    let mut evidence = PublishEvidence::new("snapcraft");
    if let Some(first) = targets.first() {
        evidence.primary_ref = Some(format!("https://snapcraft.io/{}", first.package_name));
    }
    evidence.extra = anodizer_core::PublishEvidenceExtra::Snapcraft(
        anodizer_core::publish_evidence::SnapcraftExtra {
            snapcraft_targets: targets.to_vec(),
        },
    );
    evidence
}

/// Append a `PublisherResult` for the snapcraft stage to
/// `ctx.publish_report`. Initializes the report when `None` (covers
/// `--publish` runs where the regular `PublishStage` was skipped).
/// Snapcraft is a Submitter-group publisher; `required` is the caller's
/// pre-derived [`derive_snapcraft_required`] flag.
///
/// Similar role to `stage-blob::run::record_blob_result`; signature is
/// slightly different — this recorder takes a pre-computed
/// `(evidence, outcome)` pair, while the blob recorder derives both
/// from `(uploaded, &exec_result)`. Different shape is fine; the
/// contract (init `publish_report` if `None`; push one
/// `PublisherResult` with `name="snapcraft"`,
/// `group=PublisherGroup::Submitter`) is identical.
pub(crate) fn record_snapcraft_result(
    ctx: &mut Context,
    evidence: Option<PublishEvidence>,
    outcome: PublisherOutcome,
    required: bool,
) {
    if ctx.publish_report.is_none() {
        ctx.publish_report = Some(PublishReport::default());
    }
    let report = ctx
        .publish_report
        .as_mut()
        .expect("publish_report initialized above");
    report.results.push(PublisherResult {
        name: "snapcraft".to_string(),
        group: PublisherGroup::Submitter,
        required,
        outcome,
        evidence,
    });
}

/// Derive the aggregated `required` flag for the snapcraft stage's
/// `PublisherResult`: `true` iff any selected crate's snapcraft config that
/// actually opts into publishing (`publish: true`) also sets
/// `required: true`. A `publish: false` (build-only) config's `required`
/// setting is inert here — it names an upload that will never be
/// attempted, so it must not escalate an unrelated `publish: true` config
/// in the same crate into required. Mirrors
/// `stage-blob::run::derive_blob_required` — one aggregated outcome per
/// stage, one bit per stage, so the submitter gate and the CLI's
/// required-failures exit-code gate just consult
/// `any_failed(Submitter, required_only=true)` without per-config
/// bookkeeping.
///
/// Unset (`None`) resolves to `false`: `required` only governs whether the
/// pipeline ABORTS on a failed snap upload. `verify-release`'s landing
/// check surfaces an attempted-and-failed upload as an issue regardless of
/// this flag — see `landing::run_landing_checks`.
pub(crate) fn derive_snapcraft_required(ctx: &Context) -> bool {
    let selected = &ctx.options.selected_crates;
    ctx.config
        .crate_universe()
        .into_iter()
        .filter(|c| selected.is_empty() || selected.contains(&c.name))
        .filter_map(|c| c.snapcrafts.as_ref())
        .flat_map(|configs| configs.iter())
        .filter(|cfg| cfg.publish.unwrap_or(false))
        .any(|cfg| cfg.required.unwrap_or(false))
}
