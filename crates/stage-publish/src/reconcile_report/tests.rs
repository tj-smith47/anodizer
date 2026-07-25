use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anodizer_core::context::Context;
use anodizer_core::preflight::RowKind;
use anodizer_core::test_helpers::TestContextBuilder;
use anodizer_core::{PublishEvidence, Publisher, PublisherGroup, ReconcileState};

use super::*;
use crate::testing::fake_reconciling;

/// Publisher whose probe fails outright, so the `Err` → `Unknown` mapping
/// can be exercised without a network.
struct ExplodingPublisher {
    calls: Arc<AtomicUsize>,
}

impl Publisher for ExplodingPublisher {
    fn name(&self) -> &str {
        "exploding"
    }
    fn group(&self) -> PublisherGroup {
        PublisherGroup::Submitter
    }
    fn required(&self) -> bool {
        true
    }
    fn skips_on_nightly(&self) -> bool {
        false
    }
    fn reconcile(&self, _ctx: &mut Context) -> anyhow::Result<ReconcileState> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("registry unreachable")
    }
    fn run(&self, _ctx: &mut Context) -> anyhow::Result<PublishEvidence> {
        Ok(PublishEvidence::new("exploding"))
    }
}

fn ctx() -> Context {
    TestContextBuilder::new().build()
}

fn complete(name: &str, note: &str) -> Box<dyn Publisher> {
    fake_reconciling(
        name,
        PublisherGroup::Submitter,
        true,
        ReconcileState::Complete { note: note.into() },
    )
    .0
}

fn absent(name: &str) -> Box<dyn Publisher> {
    fake_reconciling(
        name,
        PublisherGroup::Submitter,
        true,
        ReconcileState::Absent,
    )
    .0
}

fn diverged(name: &str, detail: &str) -> Box<dyn Publisher> {
    diverged_with_required(name, detail, true)
}

fn diverged_with_required(name: &str, detail: &str, required: bool) -> Box<dyn Publisher> {
    fake_reconciling(
        name,
        PublisherGroup::Submitter,
        required,
        ReconcileState::Diverged {
            detail: detail.into(),
        },
    )
    .0
}

fn unknown(name: &str, reason: &str) -> Box<dyn Publisher> {
    fake_reconciling(
        name,
        PublisherGroup::Submitter,
        true,
        ReconcileState::Unknown {
            reason: reason.into(),
        },
    )
    .0
}

/// The exit contract, stated as one assertion per state: `Diverged` is the
/// only row that blocks. `Complete` blocking would wedge every resumed
/// release (the incident this milestone exists to fix), and `Unknown`
/// blocking would let one unreachable registry veto a release.
#[test]
fn only_diverged_blocks() {
    let publishers = vec![
        complete("cargo", "version live with matching cksum"),
        absent("npm"),
        unknown("aur", "timeout connecting to AUR"),
        diverged("chocolatey", "sha256 mismatch against the published nupkg"),
    ];
    let report = ReconcileReport::probe(&publishers, &mut ctx());

    let blocking: Vec<&str> = report
        .blocking()
        .iter()
        .map(|r| r.publisher.as_str())
        .collect();
    assert_eq!(blocking, vec!["chocolatey"]);
}

/// The dispatch arm is `Diverged if p.required()`. A canary stricter than the
/// release it guards would fail runs the release completes, so an optional
/// publisher's divergence is reported without blocking.
#[test]
fn optional_divergence_is_reported_but_does_not_block() {
    let publishers = vec![
        diverged_with_required("gemfury", "sha mismatch", false),
        absent("npm"),
    ];
    let report = ReconcileReport::probe(&publishers, &mut ctx());

    assert_eq!(report.diverged().len(), 1, "still reported as diverged");
    assert!(
        report.blocking().is_empty(),
        "an optional publisher's divergence must not gate the exit code"
    );
    assert!(!report.to_json_rows()[0].blocking);
}

#[test]
fn required_divergence_blocks() {
    let publishers = vec![diverged_with_required("cargo", "sha mismatch", true)];
    let report = ReconcileReport::probe(&publishers, &mut ctx());
    assert_eq!(report.blocking().len(), 1);
    assert!(report.to_json_rows()[0].blocking);
}

#[test]
fn every_publisher_reaches_the_table() {
    let publishers = vec![
        complete("cargo", "live"),
        absent("npm"),
        unknown("aur", "x"),
    ];
    let report = ReconcileReport::probe(&publishers, &mut ctx());
    assert_eq!(report.rows.len(), 3);
    assert!(report.diverged().is_empty());
}

/// A probe that errors is `Unknown`, not a swallowed row and not an abort:
/// the sweep must continue so one broken registry cannot hide the state of
/// every publisher after it.
#[test]
fn probe_error_becomes_unknown_and_does_not_abort_the_sweep() {
    let calls = Arc::new(AtomicUsize::new(0));
    let publishers: Vec<Box<dyn Publisher>> = vec![
        Box::new(ExplodingPublisher {
            calls: calls.clone(),
        }),
        absent("npm"),
    ];
    let report = ReconcileReport::probe(&publishers, &mut ctx());

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(report.rows.len(), 2);
    assert!(report.diverged().is_empty());
    match &report.rows[0].state {
        ReconcileState::Unknown { reason } => {
            assert!(
                reason.contains("registry unreachable"),
                "probe error must survive into the reason: {reason}"
            );
        }
        other => panic!("expected Unknown from a failing probe, got {other:?}"),
    }
}

#[test]
fn only_complete_earns_the_success_marker() {
    let publishers = vec![
        complete("cargo", "live"),
        absent("npm"),
        diverged("choco", "mismatch"),
        unknown("aur", "timeout"),
    ];
    let report = ReconcileReport::probe(&publishers, &mut ctx());
    let kinds: Vec<RowKind> = report.entry_rows().into_iter().map(|(k, _)| k).collect();
    assert_eq!(
        kinds,
        vec![RowKind::Ok, RowKind::Info, RowKind::Info, RowKind::Info]
    );
}

#[test]
fn entry_rows_align_summaries_to_the_widest_publisher() {
    let publishers = vec![absent("npm"), absent("chocolatey")];
    let report = ReconcileReport::probe(&publishers, &mut ctx());
    let texts: Vec<String> = report.entry_rows().into_iter().map(|(_, t)| t).collect();

    let offsets: Vec<usize> = texts
        .iter()
        .map(|t| t.find("absent").expect("summary present"))
        .collect();
    assert_eq!(
        offsets[0], offsets[1],
        "summaries must start at the same column: {texts:?}"
    );
    assert_eq!(offsets[0], "chocolatey".len() + 2);
}

#[test]
fn entry_rows_on_an_empty_report_is_empty() {
    let report = ReconcileReport::default();
    assert!(report.entry_rows().is_empty());
    assert!(report.diverged().is_empty());
}

/// The payload an operator needs to act (which bytes differ, which PR is
/// open, why a probe was inconclusive) must survive into the row text —
/// a bare state name gives them nothing to do next.
#[test]
fn row_text_carries_the_state_payload() {
    let publishers = vec![
        complete("cargo", "0.22.2 live with matching cksum"),
        diverged("choco", "sha256 mismatch"),
        unknown("aur", "timeout connecting to AUR"),
    ];
    let report = ReconcileReport::probe(&publishers, &mut ctx());
    let rendered = report
        .entry_rows()
        .iter()
        .map(|(_, t)| t.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    for needle in [
        "complete — 0.22.2 live with matching cksum",
        "diverged — sha256 mismatch",
        "unknown — timeout connecting to AUR",
    ] {
        assert!(
            rendered.contains(needle),
            "missing {needle} in:\n{rendered}"
        );
    }
}

#[test]
fn json_rows_mark_only_diverged_as_blocking() {
    let publishers = vec![
        complete("cargo", "live"),
        absent("npm"),
        diverged("choco", "mismatch"),
        unknown("aur", "timeout"),
    ];
    let report = ReconcileReport::probe(&publishers, &mut ctx());
    let rows = report.to_json_rows();

    let tags: Vec<&str> = rows.iter().map(|r| r.state).collect();
    assert_eq!(tags, vec!["complete", "absent", "diverged", "unknown"]);

    let blocking: Vec<&str> = rows
        .iter()
        .filter(|r| r.blocking)
        .map(|r| r.publisher.as_str())
        .collect();
    assert_eq!(blocking, vec!["choco"]);

    assert_eq!(rows[1].detail, None, "Absent carries no payload");
    assert_eq!(rows[2].detail.as_deref(), Some("mismatch"));
}

#[test]
fn json_rows_serialize_without_the_absent_detail_key() {
    let publishers = vec![absent("npm")];
    let report = ReconcileReport::probe(&publishers, &mut ctx());
    let json = serde_json::to_string(&report.to_json_rows()).expect("serialize");
    assert!(
        !json.contains("detail"),
        "an Absent row must not emit a null detail key: {json}"
    );
    assert!(json.contains("\"state\":\"absent\""));
}
