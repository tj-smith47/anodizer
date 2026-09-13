//! Per-publisher post-publish landing checks.
//!
//! A publisher reporting `Succeeded` proves its client call returned OK — not
//! that consumers can actually SEE the published artifact. This module closes
//! that gap for the publishers whose published surface is independently
//! probeable with coordinates the run already recorded in its own publish
//! report:
//!
//! - **cargo** — every published crate version must be visible on the
//!   crates.io sparse index (custom registries are skipped: the crates.io
//!   index says nothing about them).
//! - **npm** — every published package version must answer a registry
//!   metadata `GET`.
//! - **pypi** — every uploaded wheel / source distribution must be listed by
//!   the index it was uploaded to, asked with the exact filename the run
//!   recorded (the configured `index_url` decides which index, so TestPyPI
//!   and a private index are probed where they were published).
//! - **blob** — every uploaded object must answer a `HEAD` through the same
//!   `ObjectStore` backend (and ambient credential chain) the upload used —
//!   buckets rarely expose a public read URL, so this is the strongest
//!   honest probe available.
//! - **snapcraft** — every uploaded snap version must be live in the Snap
//!   Store's public channel map, which is what a manual-review hold parks a
//!   revision outside of.
//! - **docker** — every image tag the run PUSHED must answer a registry
//!   manifest `GET`, and must answer with the digest the push recorded. The
//!   docker stage files no publish report, so its targets come from the
//!   artifacts themselves (`ArtifactRegistry::pushed_images`), which is what
//!   also carries them through a `--publish-only` rehydration. Having no
//!   report row, it has no `required` flag either, so its findings are
//!   routed by what they prove: an absence and a digest mismatch are
//!   definitive and fail the gate, while a registry that could not be
//!   consulted is a recorded warning — the probe reads only the plain
//!   credentials docker stored, so a repository behind a credential helper
//!   answers 401 to a push that succeeded.
//!
//! Only publishers whose recorded outcome is `Succeeded` are PROBED: a
//! skipped / deselected / rolled-back publisher published nothing this run, so
//! there is nothing to verify (and probing it would report defects the
//! publish never claimed to avoid). A probe that cannot run — network
//! failure, store build failure — is itself reported as an issue: this
//! stage's whole job is verification, and an unverifiable landing is a
//! finding, not a pass.
//!
//! A publisher that was ATTEMPTED and reported `Failed` is a different case:
//! the run tried to ship it and did not. That is a landing defect on its own
//! merits — recorded without a network probe. How a landing finding (a failed
//! publish attempt, or a probe that could not confirm the upload succeeded) is
//! reported follows the publisher's `required` flag: a REQUIRED publisher's
//! landing finding is a gate-failing issue; an advisory (`required: false`)
//! publisher's is a loud, recorded WARNING that never fails the release.
//! Non-silent — the failure still shows in this log line and in the publish
//! report's own `Failed` row that the run summary renders — while honouring
//! the operator's explicit tolerance. This stops an optional publisher from
//! stranding the required ones: in the release workflow a failed
//! verify-release skips the downstream OIDC leg, so a fatal optional landing
//! finding would block crates.io / npm / PyPI.
//!
//! This `required`-routing governs PUBLISHER landing findings only. The
//! artifact-quality / integrity gates the operator opts into elsewhere in
//! verify-release (asset-existence, install-smoke, glibc-ceiling, signature
//! crypto-verification) are NOT publisher-tolerance questions — a broken,
//! missing, or forged artifact is a release defect regardless of which
//! publisher shipped it — so they stay fatal by the operator's own opt-in.
//!
//! The probes are injected as closures so the orchestration (report
//! filtering, evidence decoding, issue wording) is unit-testable offline;
//! `VerifyReleaseStage::run` supplies the real network-backed
//! implementations.
//!
//! Every probe asks through [`probe_with_propagation`], which owns the one
//! propagation window the whole sweep shares — see
//! `.claude/rules/landing-probes-propagation.md`.

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::publish_evidence::{
    BlobTargetSnapshot, CargoYankTargetSnapshot, NpmTargetSnapshot, PublishEvidenceExtra,
    PypiFileSnapshot, SnapcraftTargetSnapshot,
};
use anodizer_core::publish_report::{PublisherOutcome, PublisherResult};

/// `(snap, version, channel)` probe signature for the Snap Store channel-map
/// check (see [`LandingProbes::snap_channel_map`]).
pub type SnapChannelMapProbe<'a> = dyn Fn(&str, &str, Option<&str>) -> anyhow::Result<bool> + 'a;

/// Image-reference probe signature for the docker registry check (see
/// [`LandingProbes::docker_manifest`]). `Ok(Some(digest))` is the content
/// digest the registry serves for the reference, `Ok(None)` a definitive
/// absence, `Err` a registry that could not be consulted.
pub type DockerManifestProbe<'a> = dyn Fn(&str) -> anyhow::Result<Option<String>> + 'a;

/// How long the landing sweep keeps asking before it reports an absence.
///
/// A registry that has ACCEPTED a publish and does not yet serve it is
/// propagating, not missing. Measured on a nine-package npm release: six
/// packages answered 404 immediately after `npm publish` returned and all nine
/// answered within a minute; crates.io sparse-index entries show the same
/// lag. So the window is sized well past the worst observed lag rather than at
/// it, and the backoff starts long enough that the first re-ask is not simply
/// the same instant again.
///
/// The window belongs to the SWEEP, not to one target. It is anchored once
/// ([`starting_now`](Self::starting_now)) and every probe shares the resulting
/// absolute [`sweep_deadline`](Self::sweep_deadline), so a registry that never
/// serves anything costs one window in total instead of one per target — a
/// 43-target release would otherwise spend over an hour inside a 20-minute
/// job.
#[derive(Debug, Clone, Copy)]
pub struct PropagationRetry {
    /// Backoff shape for the re-asks.
    pub policy: anodizer_core::retry::RetryPolicy,
    /// Wall-clock length of the sweep's window, measured from the anchor.
    pub budget: std::time::Duration,
    /// The instant the whole sweep stops re-asking, shared by every probe.
    /// `None` bounds the ladder by attempt count alone (a dry run, and the
    /// no-sleep test policies).
    pub sweep_deadline: Option<std::time::Instant>,
}

impl PropagationRetry {
    /// 5s base doubling to a 30s cap over 8 attempts inside a 3-minute window
    /// (5+10+20+30×4 = 155s of backoff).
    pub const DEFAULT: PropagationRetry = PropagationRetry {
        policy: anodizer_core::retry::RetryPolicy {
            max_attempts: 8,
            base_delay: std::time::Duration::from_secs(5),
            max_delay: std::time::Duration::from_secs(30),
        },
        budget: std::time::Duration::from_secs(180),
        sweep_deadline: None,
    };

    /// One attempt, no sleeping — for a dry run and for tests, which must
    /// exercise the orchestration without spending wall-clock time.
    pub const IMMEDIATE: PropagationRetry = PropagationRetry::immediate_attempts(1);

    /// Anchor the sweep's window at this instant, capped by `run_deadline`
    /// (the run's own `retry.max_elapsed`, already an absolute instant).
    ///
    /// Called ONCE per sweep. An operator who bounded the release's total
    /// retry time does not get a fresh window per probed target on top of it,
    /// and a wedged registry cannot multiply the window by the target count.
    pub fn starting_now(self, run_deadline: Option<std::time::Instant>) -> PropagationRetry {
        let end = std::time::Instant::now() + self.budget;
        PropagationRetry {
            sweep_deadline: Some(match run_deadline {
                Some(run) => end.min(run),
                None => end,
            }),
            ..self
        }
    }

    /// Like [`IMMEDIATE`](Self::IMMEDIATE) but with `attempts` tries and no
    /// backoff, so a test can prove a probe answering false-then-true passes
    /// without spending wall-clock time. No sweep deadline, so
    /// `deadline_exhausted` never ends the ladder before the attempts are
    /// spent; with zero delays nothing is ever slept.
    const fn immediate_attempts(attempts: u32) -> PropagationRetry {
        PropagationRetry {
            policy: anodizer_core::retry::RetryPolicy {
                max_attempts: attempts,
                base_delay: std::time::Duration::ZERO,
                max_delay: std::time::Duration::ZERO,
            },
            budget: PropagationRetry::DEFAULT.budget,
            sweep_deadline: None,
        }
    }
}

/// What a landing probe concluded once its propagation window closed.
enum Landed {
    /// The target is visible.
    Yes,
    /// Every attempt answered a definitive "absent".
    No,
    /// The probe could not reach a verdict (transport / store error).
    Unknown(anyhow::Error),
}

/// One attempt's miss, as the retry driver's per-attempt cause.
enum ProbeMiss {
    Absent,
    Failed(anyhow::Error),
}

impl std::fmt::Display for ProbeMiss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProbeMiss::Absent => write!(f, "not visible yet"),
            ProbeMiss::Failed(e) => write!(f, "{e:#}"),
        }
    }
}

/// One target's verdict plus whether reaching it needed a propagation wait.
struct ProbeOutcome {
    landed: Landed,
    /// The target answered only after more than one ask, so the per-publisher
    /// result line can say how much of the publish arrived late.
    waited: bool,
}

/// The sink the retry engine's own per-attempt lines go to while a landing
/// probe waits out propagation.
///
/// A registry serving a just-accepted publish seconds later is the expected
/// case, so those lines are execution detail and a clean release must print no
/// warning. The engine owns their wording, so the register is changed by
/// swapping the sink: a quiet logger at default verbosity, the run's own under
/// `-v`. Nothing reaches a terminal through the quiet one (its only
/// unconditional register is `error`, which the engine never uses), so it
/// needs neither the redaction cell nor the run's capture sink.
fn ladder_log(log: &StageLogger) -> StageLogger {
    if log.is_verbose() {
        log.clone()
    } else {
        StageLogger::new(crate::STAGE_NAME, anodizer_core::log::Verbosity::Quiet)
    }
}

/// Ask `probe` for `what` on `where_` until it answers yes, it answers
/// something re-asking cannot change, or the sweep's propagation window
/// closes. The single mechanism behind every landing probe in this module — a
/// per-probe copy would drift the moment one of them learned a different
/// bound.
fn probe_with_propagation(
    what: &str,
    where_: &str,
    retry: &PropagationRetry,
    log: &StageLogger,
    mut probe: impl FnMut() -> anyhow::Result<bool>,
) -> ProbeOutcome {
    let desc = format!("{what} landing probe on {where_}");
    let ladder = ladder_log(log);
    let mut asks = 0usize;
    let outcome: Result<(), ProbeMiss> = anodizer_core::retry::retry_sync_deadline(
        anodizer_core::retry::RetryLog::new(&desc, &ladder),
        &retry.policy,
        retry.sweep_deadline,
        |_attempt| {
            asks += 1;
            match probe() {
                Ok(true) => Ok(()),
                Ok(false) => Err(std::ops::ControlFlow::Continue(ProbeMiss::Absent)),
                // A failure re-asking cannot resolve — a rejected credential,
                // a store that could not be built — answers the same way every
                // time, so spending the sweep's shared window on it only
                // delays the finding and starves every target behind it.
                Err(e) if !anodizer_core::retry::is_retriable(e.as_ref()) => {
                    Err(std::ops::ControlFlow::Break(ProbeMiss::Failed(e)))
                }
                Err(e) => Err(std::ops::ControlFlow::Continue(ProbeMiss::Failed(e))),
            }
        },
    );
    ProbeOutcome {
        landed: match outcome {
            Ok(()) => Landed::Yes,
            Err(ProbeMiss::Absent) => Landed::No,
            Err(ProbeMiss::Failed(e)) => Landed::Unknown(e),
        },
        waited: asks > 1,
    }
}

/// The one trailing clause a per-publisher result line carries: how many of
/// its targets arrived late, plus whatever else that publisher has to add
/// (`" (2/9 needed a propagation wait, 1 unverifiable)"`). Empty when there is
/// nothing to add, so a clean sweep says nothing extra. One clause rather than
/// one per note — two parentheticals in a row read as two separate results.
fn result_tail(waited: usize, probed: usize, extra: Option<String>) -> String {
    let mut notes = Vec::new();
    if waited > 0 {
        notes.push(format!("{waited}/{probed} needed a propagation wait"));
    }
    notes.extend(extra);
    if notes.is_empty() {
        String::new()
    } else {
        format!(" ({})", notes.join(", "))
    }
}

/// The landing probes, injected so tests can drive the orchestration without
/// a network.
pub struct LandingProbes<'a> {
    /// How long each probe keeps asking before reporting an absence.
    pub propagation: PropagationRetry,
    /// `(crate_name, version)` → whether the version is visible on the
    /// crates.io sparse index. `Err` = the index could not be consulted.
    pub cargo_index: &'a dyn Fn(&str, &str) -> anyhow::Result<bool>,
    /// `(registry, package, version)` → whether the version answers a
    /// registry metadata GET. `Ok(false)` = a definitive 404, `Err` = the
    /// registry could not be consulted (5xx/transport) — an npm version is
    /// immutable, so an outage must not be reported as "not visible".
    pub npm_registry: &'a dyn Fn(&str, &str, &str) -> anyhow::Result<bool>,
    /// `(repository, filename)` → whether the index the file was uploaded to
    /// lists it. `Ok(false)` = the index answered without the file, `Err` =
    /// the index could not be consulted — a PyPI filename is a permanent slot
    /// that can never be re-uploaded, so an outage must not read as an
    /// absence.
    pub pypi_index: &'a dyn Fn(&str, &str) -> anyhow::Result<bool>,
    /// Blob target → whether the object exists in its bucket. `Err` = the
    /// store could not be built or the HEAD failed indeterminately.
    pub blob_head: &'a dyn Fn(&BlobTargetSnapshot) -> anyhow::Result<bool>,
    /// `(snap, version, channel)` → whether the version is live in the Snap
    /// Store's channel map (in the given channel, or any channel when
    /// `None`). `Ok(false)` covers snap-unknown and version-absent alike;
    /// `Err` = the store could not be consulted.
    pub snap_channel_map: &'a SnapChannelMapProbe<'a>,
    /// Image reference → the content digest the registry serves for it.
    /// `Ok(None)` = the registry answered that the reference does not exist;
    /// `Err` = the registry could not be consulted, which must read as
    /// unverifiable rather than as an absence (a private repository whose
    /// credential the probe lacks is still live for everyone holding one).
    pub docker_manifest: &'a DockerManifestProbe<'a>,
}

/// Run every applicable landing check against the run's publish report.
///
/// Returns the number of publishers actually probed, so the caller can tell
/// "everything verified" apart from "nothing was in scope to verify" when
/// deciding whether to stamp a verdict.
pub(crate) fn run_landing_checks(
    ctx: &Context,
    log: &StageLogger,
    probes: &LandingProbes<'_>,
    issues: &mut Vec<String>,
) -> usize {
    let mut probed_publishers = 0usize;
    if check_docker_landing(ctx, log, probes, issues) {
        probed_publishers += 1;
    }
    let Some(report) = ctx.publish_report() else {
        log.verbose("no publish report recorded this run — report-driven landing checks skipped");
        return probed_publishers;
    };
    for result in &report.results {
        // A publisher's landing findings are routed by its `required` flag: a
        // required publisher's finding fails the gate; an advisory
        // (`required: false`) publisher's is a loud, recorded warning that
        // never fails the release — so an optional publisher can neither
        // silently pass nor, by failing, block the required ones (a fatal
        // verify-release skips the downstream OIDC leg, stranding crates.io /
        // npm / PyPI). Operator-enabled artifact-quality gates elsewhere in
        // this stage stay fatal regardless; see the module doc.
        let mut findings: Vec<String> = Vec::new();
        let probed = if let PublisherOutcome::Succeeded = result.outcome {
            match result.name.as_str() {
                "cargo" => check_cargo_landing(result, log, probes, &mut findings),
                "npm" => check_npm_landing(result, log, probes, &mut findings),
                "pypi" => check_pypi_landing(result, log, probes, &mut findings),
                "blob" => check_blob_landing(result, log, probes, &mut findings),
                "snapcraft" => check_snapcraft_landing(result, log, probes, &mut findings),
                _ => false,
            }
        } else if let PublisherOutcome::Failed(reason) = &result.outcome {
            // A publisher the run actually attempted and failed is a landing
            // defect on its own merits (no probe needed) — every publisher,
            // not just the network-probed ones.
            log.warn(&format!(
                "{} publish attempt failed this run — landing not verified: {reason}",
                result.name
            ));
            findings.push(format!(
                "{}: publish attempt failed this run — {reason}",
                result.name
            ));
            false
        } else {
            log.verbose(&format!(
                "skipped {} landing check — publisher did not succeed this run ({:?})",
                result.name, result.outcome
            ));
            false
        };
        if result.required {
            issues.extend(findings);
        } else {
            for finding in findings {
                log.warn(&format!(
                    "optional publisher not gating the release (required: false) — {finding}"
                ));
            }
        }
        if probed {
            probed_publishers += 1;
        }
    }
    probed_publishers
}

/// Decode the cargo publisher's recorded publish targets.
fn cargo_targets(result: &PublisherResult) -> &[CargoYankTargetSnapshot] {
    match result.evidence.as_ref().map(|e| &e.extra) {
        Some(PublishEvidenceExtra::Cargo(extra)) => &extra.cargo_yank_targets,
        _ => &[],
    }
}

/// Decode the npm publisher's recorded publish targets.
fn npm_targets(result: &PublisherResult) -> &[NpmTargetSnapshot] {
    match result.evidence.as_ref().map(|e| &e.extra) {
        Some(PublishEvidenceExtra::Npm(extra)) => &extra.npm_targets,
        _ => &[],
    }
}

/// Decode the blob publisher's recorded upload targets.
fn blob_targets(result: &PublisherResult) -> &[BlobTargetSnapshot] {
    match result.evidence.as_ref().map(|e| &e.extra) {
        Some(PublishEvidenceExtra::Blob(extra)) => &extra.blob_targets,
        _ => &[],
    }
}

/// Probe every crates.io-targeted crate the cargo publisher recorded.
/// Returns whether at least one target was probed.
fn check_cargo_landing(
    result: &PublisherResult,
    log: &StageLogger,
    probes: &LandingProbes<'_>,
    issues: &mut Vec<String>,
) -> bool {
    let targets = cargo_targets(result);
    if targets.is_empty() {
        log.verbose("cargo succeeded but recorded no published crates — nothing to probe");
        return false;
    }
    let mut visible: Vec<String> = Vec::new();
    let mut probed = 0usize;
    let mut waited = 0usize;
    for t in targets {
        // A custom registry/index means the crates.io sparse index is not
        // authoritative for this target — the same scoping the publisher's
        // own idempotency guard applies.
        if t.registry.is_some() || t.index.is_some() {
            log.verbose(&format!(
                "skipped index probe for {}@{} — published to a non-crates.io registry",
                t.name, t.version
            ));
            continue;
        }
        probed += 1;
        let coords = format!("cargo: {}@{}", t.name, t.version);
        let outcome = probe_with_propagation(
            &coords,
            "the crates.io index",
            &probes.propagation,
            log,
            || (probes.cargo_index)(&t.name, &t.version),
        );
        waited += usize::from(outcome.waited);
        match outcome.landed {
            Landed::Yes => visible.push(format!("{}@{}", t.name, t.version)),
            Landed::No => issues.push(format!(
                "cargo: {}@{} reported published but is not visible on the \
                 crates.io index",
                t.name, t.version
            )),
            Landed::Unknown(e) => issues.push(format!(
                "cargo: could not probe the crates.io index for {}@{}: {e:#}",
                t.name, t.version
            )),
        }
    }
    if probed > 0 && visible.len() == probed {
        let tail = result_tail(waited, probed, None);
        if probed == 1 {
            log.status(&format!(
                "cargo: {} visible on crates.io index{tail}",
                visible[0]
            ));
        } else {
            log.status(&format!(
                "cargo: {probed}/{probed} published crate(s) visible on crates.io index{tail}"
            ));
        }
    }
    probed > 0
}

/// Trim the URL scheme off a registry endpoint for concise status wording.
fn registry_host(registry: &str) -> &str {
    registry
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
}

/// Probe every package version the npm publisher recorded.
/// Returns whether at least one target was probed.
fn check_npm_landing(
    result: &PublisherResult,
    log: &StageLogger,
    probes: &LandingProbes<'_>,
    issues: &mut Vec<String>,
) -> bool {
    let targets = npm_targets(result);
    if targets.is_empty() {
        log.verbose("npm succeeded but recorded no published packages — nothing to probe");
        return false;
    }
    let mut visible: Vec<String> = Vec::new();
    let mut waited = 0usize;
    for t in targets {
        let coords = format!("npm: {}@{}", t.package, t.version);
        let outcome = probe_with_propagation(
            &coords,
            registry_host(&t.registry),
            &probes.propagation,
            log,
            || (probes.npm_registry)(&t.registry, &t.package, &t.version),
        );
        waited += usize::from(outcome.waited);
        match outcome.landed {
            Landed::Yes => visible.push(format!("{}@{}", t.package, t.version)),
            Landed::No => issues.push(format!(
                "npm: {}@{} reported published but is not visible on {}",
                t.package,
                t.version,
                registry_host(&t.registry)
            )),
            // Indeterminate: the registry could not be consulted. An npm
            // version is immutable once published, so a transient outage must
            // fail closed as "unverifiable", never as "not visible" — the
            // latter would fail an already published one-way-door release.
            Landed::Unknown(e) => issues.push(format!(
                "npm: could not confirm {}@{} on {}: {e:#}",
                t.package,
                t.version,
                registry_host(&t.registry)
            )),
        }
    }
    if visible.len() == targets.len() {
        let tail = result_tail(waited, targets.len(), None);
        // One npm family can span a public and a private registry, so naming
        // only the first would credit another registry's packages to it.
        let mut hosts: Vec<String> = targets
            .iter()
            .map(|t| registry_host(&t.registry).to_string())
            .collect();
        hosts.sort();
        hosts.dedup();
        let host = hosts.join(", ");
        if targets.len() == 1 {
            log.status(&format!("npm: {} visible on {host}{tail}", visible[0]));
        } else {
            log.status(&format!(
                "npm: {0}/{0} published package(s) visible on {host}{tail}",
                targets.len()
            ));
        }
    }
    true
}

/// Decode the pypi publisher's recorded uploaded files.
fn pypi_targets(result: &PublisherResult) -> &[PypiFileSnapshot] {
    match result.evidence.as_ref().map(|e| &e.extra) {
        Some(PublishEvidenceExtra::Pypi(extra)) => &extra.pypi_files,
        _ => &[],
    }
}

/// Probe every file the pypi publisher recorded against the index it was
/// uploaded to. Returns whether at least one target was probed.
///
/// Probed per FILE, not per version: a release ships one wheel per platform
/// and the index accepts them one at a time, so a partial upload leaves the
/// version present and one platform's wheel missing — which a version-level
/// question would report as complete.
fn check_pypi_landing(
    result: &PublisherResult,
    log: &StageLogger,
    probes: &LandingProbes<'_>,
    issues: &mut Vec<String>,
) -> bool {
    let targets = pypi_targets(result);
    if targets.is_empty() {
        log.verbose("pypi succeeded but recorded no uploaded files — nothing to probe");
        return false;
    }
    let mut visible = 0usize;
    let mut waited = 0usize;
    for t in targets {
        let coords = format!("pypi: {}", t.filename);
        let index = index_host(&t.repository);
        let outcome = probe_with_propagation(&coords, &index, &probes.propagation, log, || {
            (probes.pypi_index)(&t.repository, &t.filename)
        });
        waited += usize::from(outcome.waited);
        match outcome.landed {
            Landed::Yes => visible += 1,
            Landed::No => issues.push(format!(
                "pypi: {} reported uploaded but is not listed on {index}",
                t.filename
            )),
            // A PyPI filename is a permanent index slot that can never be
            // re-uploaded, so an index that could not be consulted must read
            // as unverifiable rather than as an absence: the latter would fail
            // a release whose files are live and cannot be re-published.
            Landed::Unknown(e) => issues.push(format!(
                "pypi: could not confirm {} on {index}: {e:#}",
                t.filename
            )),
        }
    }
    if visible == targets.len() {
        let tail = result_tail(waited, targets.len(), None);
        // A run can upload to more than one index (a TestPyPI leg beside
        // pypi.org), so naming only the first index would credit files from
        // another index to it.
        let mut indexes: Vec<String> = targets.iter().map(|t| index_host(&t.repository)).collect();
        indexes.sort();
        indexes.dedup();
        let index = indexes.join(", ");
        if targets.len() == 1 {
            log.status(&format!(
                "pypi: {} listed on {index}{tail}",
                targets[0].filename
            ));
        } else {
            log.status(&format!(
                "pypi: {visible}/{} uploaded file(s) listed on {index}{tail}",
                targets.len()
            ));
        }
    }
    true
}

/// Name the index a pypi upload endpoint belongs to, for status wording:
/// `https://upload.pypi.org/legacy/` reads as `pypi.org`.
fn index_host(repository: &str) -> String {
    reqwest::Url::parse(repository)
        .ok()
        .and_then(|u| {
            u.host_str()
                .map(|h| h.trim_start_matches("upload.").to_string())
        })
        .unwrap_or_else(|| registry_host(repository).to_string())
}

/// HEAD every object the blob publisher recorded.
/// Returns whether at least one target was probed.
fn check_blob_landing(
    result: &PublisherResult,
    log: &StageLogger,
    probes: &LandingProbes<'_>,
    issues: &mut Vec<String>,
) -> bool {
    let targets = blob_targets(result);
    if targets.is_empty() {
        log.verbose("blob succeeded but recorded no uploaded objects — nothing to probe");
        return false;
    }
    let mut present = 0usize;
    let mut waited = 0usize;
    for t in targets {
        let url = format!("{}://{}/{}", t.provider, t.bucket, t.key);
        let coords = format!("blob: {url}");
        let outcome =
            probe_with_propagation(&coords, "the bucket", &probes.propagation, log, || {
                (probes.blob_head)(t)
            });
        waited += usize::from(outcome.waited);
        match outcome.landed {
            Landed::Yes => {
                present += 1;
                log.verbose(&format!("{url} present"));
            }
            Landed::No => issues.push(format!(
                "blob: {url} reported uploaded but is missing from the bucket"
            )),
            Landed::Unknown(e) => issues.push(format!("blob: could not verify {url}: {e:#}")),
        }
    }
    if present == targets.len() {
        let tail = result_tail(waited, targets.len(), None);
        // A run can upload to more than one bucket, so naming only the first
        // would credit another bucket's objects to it. The bucket is the host
        // half of the line, so the subject half names the key alone rather
        // than repeating it.
        let mut buckets: Vec<String> = targets
            .iter()
            .map(|t| format!("{}://{}", t.provider, t.bucket))
            .collect();
        buckets.sort();
        buckets.dedup();
        let buckets = buckets.join(", ");
        if targets.len() == 1 {
            log.status(&format!(
                "blob: {} visible in {buckets}{tail}",
                targets[0].key
            ));
        } else {
            log.status(&format!(
                "blob: {present}/{} uploaded object(s) visible in {buckets}{tail}",
                targets.len()
            ));
        }
    }
    true
}

/// Decode the snapcraft publisher's recorded upload targets.
fn snapcraft_targets(result: &PublisherResult) -> &[SnapcraftTargetSnapshot] {
    match result.evidence.as_ref().map(|e| &e.extra) {
        Some(PublishEvidenceExtra::Snapcraft(extra)) => &extra.snapcraft_targets,
        _ => &[],
    }
}

/// Probe the Snap Store channel map for every snap the snapcraft publisher
/// recorded. Returns whether at least one target was probed.
///
/// A `snapcraft upload` OK proves acceptance, not delivery: a manual-review
/// hold parks the revision outside every channel until a human approves it,
/// and a decline arrives only by email — so an absent version is reported as
/// an issue either way, with the hold context in the wording when the run
/// recorded one. A held snap that review has since approved probes visible
/// and passes cleanly.
fn check_snapcraft_landing(
    result: &PublisherResult,
    log: &StageLogger,
    probes: &LandingProbes<'_>,
    issues: &mut Vec<String>,
) -> bool {
    let targets = snapcraft_targets(result);
    if targets.is_empty() {
        log.verbose("snapcraft succeeded but recorded no uploaded snaps — nothing to probe");
        return false;
    }
    let mut visible: Vec<String> = Vec::new();
    let mut probed = 0usize;
    let mut waited = 0usize;
    // A dual-arch snap records one evidence entry per architecture, but the
    // store channel-map probe is arch-independent (it asks whether a version
    // is live in a channel), so probing every arch entry would query the same
    // listing twice. Collapse to one probe per (package, version, channel).
    let mut seen: std::collections::HashSet<(String, String, Option<String>)> =
        std::collections::HashSet::new();
    for t in targets {
        // Pre-`version` snapshots (written by older anodizer versions) carry
        // no version to look for — nothing honest to probe.
        let Some(version) = t.version.as_deref() else {
            log.verbose(&format!(
                "skipped store probe for snap '{}' — no version recorded in the run snapshot",
                t.package_name
            ));
            continue;
        };
        if !seen.insert((
            t.package_name.clone(),
            version.to_string(),
            t.channel.clone(),
        )) {
            continue;
        }
        probed += 1;
        let coords = format!("{} {version}", t.package_name);
        let labelled = format!("snapcraft: {coords}");
        let outcome = probe_with_propagation(
            &labelled,
            "the Snap Store channel map",
            &probes.propagation,
            log,
            || (probes.snap_channel_map)(&t.package_name, version, t.channel.as_deref()),
        );
        waited += usize::from(outcome.waited);
        match outcome.landed {
            Landed::Yes => visible.push(coords),
            Landed::No if t.held_for_review => issues.push(format!(
                "snapcraft: {coords} was HELD for Snap Store manual review and is not live in \
                 the store — consumers get nothing until review approves \
                 (https://dashboard.snapcraft.io/snaps/{}/)",
                t.package_name
            )),
            Landed::No => issues.push(format!(
                "snapcraft: {coords} reported uploaded but is not in the store's channel map{}",
                t.channel
                    .as_deref()
                    .map(|c| format!(" for channel '{c}'"))
                    .unwrap_or_default()
            )),
            Landed::Unknown(e) => issues.push(format!(
                "snapcraft: could not probe the Snap Store for {coords}: {e:#}"
            )),
        }
    }
    if probed > 0 && visible.len() == probed {
        let tail = result_tail(waited, probed, None);
        if probed == 1 {
            log.status(&format!(
                "snapcraft: {} live in the Snap Store channel map{tail}",
                visible[0]
            ));
        } else {
            log.status(&format!(
                "snapcraft: {probed}/{probed} uploaded snap(s) live in the Snap Store \
                 channel map{tail}"
            ));
        }
    }
    probed > 0
}

/// How many asks a digest mismatch gets before the ladder ends. One re-ask
/// covers a registry still catching up on an overwrite; the shared
/// propagation window belongs to every remaining target.
const MISMATCH_ASKS: usize = 2;

/// Probe the registry for every image reference this run PUSHED.
/// Returns whether at least one reference was probed.
///
/// Docker is not a `publish_report` participant, so the targets come from the
/// artifacts: an image artifact carries the pushed marker exactly when its
/// push returned OK, which leaves a snapshot, a dry run and a `skip_push:`
/// manifest out and survives a `--publish-only` rehydration from the
/// preserved `artifacts.json`.
///
/// A reference whose push recorded a digest is held to it: a registry serving
/// the tag at DIFFERENT content means the tag this release named now resolves
/// to an image the release did not build, which a plain presence question
/// would pass.
fn check_docker_landing(
    ctx: &Context,
    log: &StageLogger,
    probes: &LandingProbes<'_>,
    issues: &mut Vec<String>,
) -> bool {
    // Every other axis is gated by its own publisher's selection. Docker's
    // targets come from artifacts that carry their pushed marker across a
    // `--publish-only` rehydration, so without this gate a leg told to leave
    // the registry alone would still ask it.
    if ctx.publisher_deselected("docker") {
        log.verbose(&ctx.deselected_reason("docker"));
        return false;
    }
    let pushed = ctx.artifacts.pushed_images();
    if pushed.is_empty() {
        log.verbose("no image was pushed to a registry this run — nothing to probe");
        return false;
    }
    let mut visible: Vec<String> = Vec::new();
    let mut unverifiable = 0usize;
    let mut waited = 0usize;
    for image in &pushed {
        let coords = format!("docker: {}", image.reference);
        let registry = crate::registry::image_registry(&image.reference);
        // What the last ask actually saw, so an absence can be told apart
        // from a tag that resolves to different content without asking the
        // registry a second time.
        let served: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);
        let mismatched_asks = std::cell::Cell::new(0usize);
        let outcome =
            probe_with_propagation(
                &coords,
                &registry,
                &probes.propagation,
                log,
                || match (probes.docker_manifest)(&image.reference) {
                    Ok(Some(digest)) => {
                        let matched = image
                            .digest
                            .as_deref()
                            .is_none_or(|want| crate::registry::digests_match(want, &digest));
                        *served.borrow_mut() = Some(digest);
                        if !matched {
                            mismatched_asks.set(mismatched_asks.get() + 1);
                            // A tag serving other content is a fixed answer,
                            // not propagation. One re-ask covers a registry
                            // still catching up on an overwrite; past that,
                            // re-asking only spends the window every other
                            // target is queued behind.
                            anyhow::ensure!(
                                mismatched_asks.get() < MISMATCH_ASKS,
                                "the registry serves this tag at a digest \
                                 other than the one pushed"
                            );
                        }
                        Ok(matched)
                    }
                    Ok(None) => {
                        *served.borrow_mut() = None;
                        Ok(false)
                    }
                    Err(e) => Err(e),
                },
            );
        waited += usize::from(outcome.waited);
        let got = served.into_inner();
        let want = image.digest.as_deref();
        let mismatched = match (got.as_deref(), want) {
            (Some(got), Some(want)) => !crate::registry::digests_match(want, got),
            _ => false,
        };
        match outcome.landed {
            Landed::Yes => visible.push(image.reference.clone()),
            // The tag this release named now resolves to an image the release
            // did not build — definitive whichever way the ladder ended.
            _ if mismatched => issues.push(format!(
                "docker: {} was pushed as {} but {registry} serves {}",
                image.reference,
                want.unwrap_or_default(),
                got.as_deref().unwrap_or_default()
            )),
            Landed::No => issues.push(format!(
                "docker: {} reported pushed but is not visible on {registry}",
                image.reference
            )),
            // A registry that could not be consulted is not an absence: the
            // probe reads only the plain credentials docker stored, so a
            // private repository behind a credential helper answers 401 to a
            // push that succeeded. Reported the way an optional
            // publisher's landing finding is — loud, recorded, and never a
            // reason to fail a release whose one-way-door publishers have
            // already run.
            Landed::Unknown(e) => {
                unverifiable += 1;
                log.warn(&format!(
                    "unverifiable docker landing not gating the release — docker: could not \
                     confirm {} on {registry}: {e:#}",
                    image.reference
                ));
            }
        }
    }
    // An image the probe could not reach is a warning, not a finding, so the
    // run passes — and the images that DID verify must still be reported.
    if visible.len() + unverifiable == pushed.len() {
        let tail = result_tail(
            waited,
            pushed.len(),
            (unverifiable > 0).then(|| format!("{unverifiable} unverifiable")),
        );
        // A run can push to more than one registry, so naming only the first
        // would credit another registry's images to it.
        let mut registries: Vec<String> = pushed
            .iter()
            .map(|i| crate::registry::image_registry(&i.reference))
            .collect();
        registries.sort();
        registries.dedup();
        let registries = registries.join(", ");
        if pushed.len() == 1 && unverifiable == 0 {
            log.status(&format!(
                "docker: {} visible on {registries}{tail}",
                visible[0]
            ));
        } else {
            log.status(&format!(
                "docker: {}/{} pushed image(s) visible on {registries}{tail}",
                visible.len(),
                pushed.len()
            ));
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::Config;
    use anodizer_core::context::ContextOptions;
    use anodizer_core::publish_evidence::{BlobExtra, CargoExtra, NpmExtra, PublishEvidence};
    use anodizer_core::publish_report::{PublishReport, PublisherGroup, SkipReason};
    use std::cell::Cell;

    fn ctx_with_report(report: PublishReport) -> Context {
        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        ctx.set_publish_report(report);
        ctx
    }

    fn result_with(
        name: &str,
        outcome: PublisherOutcome,
        extra: PublishEvidenceExtra,
    ) -> PublisherResult {
        let mut evidence = PublishEvidence::new(name);
        evidence.extra = extra;
        PublisherResult {
            name: name.to_string(),
            group: PublisherGroup::Submitter,
            required: true,
            outcome,
            evidence: Some(evidence),
            entry_skips: Vec::new(),
        }
    }

    fn cargo_extra(targets: &[(&str, &str)]) -> PublishEvidenceExtra {
        PublishEvidenceExtra::Cargo(CargoExtra {
            cargo_yank_targets: targets
                .iter()
                .map(|(n, v)| CargoYankTargetSnapshot {
                    name: n.to_string(),
                    version: v.to_string(),
                    registry: None,
                    index: None,
                })
                .collect(),
        })
    }

    fn npm_extra(targets: &[(&str, &str)]) -> PublishEvidenceExtra {
        PublishEvidenceExtra::Npm(NpmExtra {
            npm_targets: targets
                .iter()
                .map(|(p, v)| NpmTargetSnapshot {
                    target: p.to_string(),
                    package: p.to_string(),
                    version: v.to_string(),
                    registry: "https://registry.npmjs.org".to_string(),
                    dist_tag: "latest".to_string(),
                    ..Default::default()
                })
                .collect(),
        })
    }

    fn blob_extra(keys: &[&str]) -> PublishEvidenceExtra {
        PublishEvidenceExtra::Blob(BlobExtra {
            blob_targets: keys
                .iter()
                .map(|k| BlobTargetSnapshot {
                    provider: "s3".to_string(),
                    bucket: "bkt".to_string(),
                    key: k.to_string(),
                    region: None,
                    endpoint: None,
                })
                .collect(),
        })
    }

    /// A context whose loggers record every line, for asserting the
    /// propagation status line.
    fn ctx_capturing(report: PublishReport) -> (Context, anodizer_core::log::LogCapture) {
        let capture = anodizer_core::log::LogCapture::new();
        let mut ctx = ctx_with_report(report);
        ctx.with_log_capture(capture.clone());
        (ctx, capture)
    }

    /// The default-visible `status` lines a capture recorded, in order.
    fn statuses(capture: &anodizer_core::log::LogCapture) -> Vec<String> {
        capture
            .all_messages()
            .into_iter()
            .filter(|(l, _)| *l == anodizer_core::log::LogLevel::Status)
            .map(|(_, m)| m)
            .collect()
    }

    /// A probe that answers each of `answers` in turn, then repeats the last.
    /// `None` is an indeterminate probe failure.
    fn scripted(answers: Vec<Option<bool>>) -> impl Fn() -> anyhow::Result<bool> {
        let calls = Cell::new(0usize);
        move || {
            let i = calls.get().min(answers.len() - 1);
            calls.set(calls.get() + 1);
            match answers[i] {
                Some(v) => Ok(v),
                None => anyhow::bail!("connection reset by peer"),
            }
        }
    }

    /// The retry policy every propagation test drives: several attempts, no
    /// sleeping, so the orchestration is exercised in microseconds.
    const FLAKY: PropagationRetry = PropagationRetry::immediate_attempts(5);

    /// A registry that accepted a publish and has not served it yet is
    /// propagating. The regression this closes: six of nine npm packages
    /// answered 404 on the first probe and all nine were visible a minute
    /// later, so a single-shot probe failed a release whose every artifact
    /// was published.
    #[test]
    fn npm_probe_retries_until_the_registry_serves_the_version() {
        let report = PublishReport {
            results: vec![result_with(
                "npm",
                PublisherOutcome::Succeeded,
                npm_extra(&[("demo", "1.0.0")]),
            )],
            ..Default::default()
        };
        let (ctx, capture) = ctx_capturing(report);
        let log = test_logger(&ctx);
        let answers = scripted(vec![Some(false), Some(false), Some(true)]);
        let npm = |_: &str, _: &str, _: &str| answers();
        let probes = LandingProbes {
            propagation: FLAKY,
            npm_registry: &npm,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(
            capture.warn_count(),
            0,
            "a release whose packages all arrived must print no warning: {:?}",
            capture.warn_messages()
        );
        let lines = statuses(&capture);
        assert!(
            lines.iter().any(|m| m.starts_with(
                "npm: demo@1.0.0 visible on registry.npmjs.org (1/1 needed a propagation wait"
            )),
            "the result line carries the propagation count: {lines:?}"
        );
    }

    #[test]
    fn npm_probe_reports_the_absence_once_the_window_closes() {
        let report = PublishReport {
            results: vec![result_with(
                "npm",
                PublisherOutcome::Succeeded,
                npm_extra(&[("demo", "1.0.0")]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let npm = |_: &str, _: &str, _: &str| Ok(false);
        let probes = LandingProbes {
            propagation: FLAKY,
            npm_registry: &npm,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].contains("demo@1.0.0") && issues[0].contains("is not visible"),
            "the wording after the window closes is unchanged: {issues:?}"
        );
    }

    #[test]
    fn npm_probe_error_then_success_is_not_an_issue() {
        let report = PublishReport {
            results: vec![result_with(
                "npm",
                PublisherOutcome::Succeeded,
                npm_extra(&[("demo", "1.0.0")]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let answers = scripted(vec![None, Some(true)]);
        let npm = |_: &str, _: &str, _: &str| answers();
        let probes = LandingProbes {
            propagation: FLAKY,
            npm_registry: &npm,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn cargo_probe_retries_a_propagating_sparse_index() {
        let report = PublishReport {
            results: vec![result_with(
                "cargo",
                PublisherOutcome::Succeeded,
                cargo_extra(&[("app-core", "1.0.0")]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let answers = scripted(vec![Some(false), Some(false), Some(true)]);
        let cargo = |_: &str, _: &str| answers();
        let probes = LandingProbes {
            propagation: FLAKY,
            cargo_index: &cargo,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn cargo_probe_reports_the_absence_once_the_window_closes() {
        let report = PublishReport {
            results: vec![result_with(
                "cargo",
                PublisherOutcome::Succeeded,
                cargo_extra(&[("app-core", "1.0.0")]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let cargo = |_: &str, _: &str| Ok(false);
        let probes = LandingProbes {
            propagation: FLAKY,
            cargo_index: &cargo,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].contains("app-core@1.0.0") && issues[0].contains("not visible"),
            "{issues:?}"
        );
    }

    #[test]
    fn cargo_probe_error_then_success_is_not_an_issue() {
        let report = PublishReport {
            results: vec![result_with(
                "cargo",
                PublisherOutcome::Succeeded,
                cargo_extra(&[("app-core", "1.0.0")]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let answers = scripted(vec![None, Some(true)]);
        let cargo = |_: &str, _: &str| answers();
        let probes = LandingProbes {
            propagation: FLAKY,
            cargo_index: &cargo,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn blob_probe_retries_an_object_the_bucket_has_not_served_yet() {
        let report = PublishReport {
            results: vec![result_with(
                "blob",
                PublisherOutcome::Succeeded,
                blob_extra(&["dist/demo.tar.gz"]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let answers = scripted(vec![Some(false), Some(false), Some(true)]);
        let blob = |_: &BlobTargetSnapshot| answers();
        let probes = LandingProbes {
            propagation: FLAKY,
            blob_head: &blob,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn blob_probe_reports_the_absence_once_the_window_closes() {
        let report = PublishReport {
            results: vec![result_with(
                "blob",
                PublisherOutcome::Succeeded,
                blob_extra(&["dist/demo.tar.gz"]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let blob = |_: &BlobTargetSnapshot| Ok(false);
        let probes = LandingProbes {
            propagation: FLAKY,
            blob_head: &blob,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("missing from the bucket"), "{issues:?}");
    }

    #[test]
    fn blob_probe_error_then_success_is_not_an_issue() {
        let report = PublishReport {
            results: vec![result_with(
                "blob",
                PublisherOutcome::Succeeded,
                blob_extra(&["dist/demo.tar.gz"]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let answers = scripted(vec![None, Some(true)]);
        let blob = |_: &BlobTargetSnapshot| answers();
        let probes = LandingProbes {
            propagation: FLAKY,
            blob_head: &blob,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn snapcraft_probe_retries_a_channel_map_that_has_not_caught_up() {
        let report = PublishReport {
            results: vec![result_with(
                "snapcraft",
                PublisherOutcome::Succeeded,
                snapcraft_extra(&[("demo", "1.2.3", Some("stable"), false)]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let answers = scripted(vec![Some(false), Some(false), Some(true)]);
        let snap = |_: &str, _: &str, _: Option<&str>| answers();
        let probes = LandingProbes {
            propagation: FLAKY,
            snap_channel_map: &snap,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn snapcraft_probe_reports_the_absence_once_the_window_closes() {
        let report = PublishReport {
            results: vec![result_with(
                "snapcraft",
                PublisherOutcome::Succeeded,
                snapcraft_extra(&[("demo", "1.2.3", Some("stable"), false)]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let snap = |_: &str, _: &str, _: Option<&str>| Ok(false);
        let probes = LandingProbes {
            propagation: FLAKY,
            snap_channel_map: &snap,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].contains("not in the store's channel map"),
            "{issues:?}"
        );
    }

    #[test]
    fn snapcraft_probe_error_then_success_is_not_an_issue() {
        let report = PublishReport {
            results: vec![result_with(
                "snapcraft",
                PublisherOutcome::Succeeded,
                snapcraft_extra(&[("demo", "1.2.3", Some("stable"), false)]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let answers = scripted(vec![None, Some(true)]);
        let snap = |_: &str, _: &str, _: Option<&str>| answers();
        let probes = LandingProbes {
            propagation: FLAKY,
            snap_channel_map: &snap,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert!(issues.is_empty(), "{issues:?}");
    }

    /// Probes that must never fire — for paths that filter before probing.
    fn panicking_probes() -> LandingProbes<'static> {
        LandingProbes {
            propagation: PropagationRetry::IMMEDIATE,
            cargo_index: &|n, v| panic!("cargo probe must not fire for {n}@{v}"),
            npm_registry: &|_, p, v| panic!("npm probe must not fire for {p}@{v}"),
            pypi_index: &|_, f| panic!("pypi probe must not fire for {f}"),
            blob_head: &|t| panic!("blob probe must not fire for {}", t.key),
            snap_channel_map: &|s, v, _| panic!("snap probe must not fire for {s} {v}"),
            docker_manifest: &|i| panic!("docker probe must not fire for {i}"),
        }
    }

    /// A context carrying image artifacts as the docker stage registered
    /// them, with no publish report — docker files none.
    fn ctx_with_pushed_images(
        images: &[(&str, Option<&str>, bool)],
    ) -> (Context, anodizer_core::log::LogCapture) {
        let capture = anodizer_core::log::LogCapture::new();
        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        ctx.with_log_capture(capture.clone());
        for (reference, digest, pushed) in images {
            let mut metadata = std::collections::HashMap::new();
            metadata.insert("tag".to_string(), (*reference).to_string());
            if let Some(d) = digest {
                metadata.insert("digest".to_string(), (*d).to_string());
            }
            if *pushed {
                metadata.insert(
                    anodizer_core::artifact::PUSHED_META.to_string(),
                    anodizer_core::artifact::PUSHED_VALUE.to_string(),
                );
            }
            ctx.artifacts.add(anodizer_core::artifact::Artifact {
                kind: anodizer_core::artifact::ArtifactKind::DockerImageV2,
                name: (*reference).to_string(),
                path: std::path::PathBuf::from(*reference),
                target: None,
                crate_name: "app".to_string(),
                metadata,
                size: None,
            });
        }
        (ctx, capture)
    }

    fn test_logger(ctx: &Context) -> StageLogger {
        ctx.logger("verify-release")
    }

    #[test]
    fn no_publish_report_probes_nothing() {
        let ctx = Context::new(Config::default(), ContextOptions::default());
        let log = test_logger(&ctx);
        let mut issues = Vec::new();
        let probed = run_landing_checks(&ctx, &log, &panicking_probes(), &mut issues);
        assert_eq!(probed, 0);
        assert!(issues.is_empty());
    }

    #[test]
    fn skipped_and_rolled_back_publishers_are_never_probed_or_flagged() {
        // Genuine skips / an intentionally-reverted publish published nothing
        // this run — no probe, no issue.
        let report = PublishReport {
            results: vec![
                result_with(
                    "npm",
                    PublisherOutcome::Skipped(SkipReason::Deselected),
                    npm_extra(&[("app", "1.0.0")]),
                ),
                result_with("blob", PublisherOutcome::RolledBack, blob_extra(&["k"])),
            ],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let mut issues = Vec::new();
        let probed = run_landing_checks(&ctx, &log, &panicking_probes(), &mut issues);
        assert_eq!(probed, 0);
        assert!(issues.is_empty());
    }

    #[test]
    fn attempted_and_failed_publisher_is_reported_as_a_landing_issue() {
        // A publisher the run actually tried to ship and failed is a landing
        // defect on its own — no probe needed, and it must not be silently
        // swallowed like a genuine skip.
        let report = PublishReport {
            results: vec![result_with(
                "cargo",
                PublisherOutcome::Failed("boom".into()),
                cargo_extra(&[("app", "1.0.0")]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let mut issues = Vec::new();
        let probed = run_landing_checks(&ctx, &log, &panicking_probes(), &mut issues);
        assert_eq!(probed, 0);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("cargo"));
        assert!(issues[0].contains("boom"));
    }

    #[test]
    fn failed_snapcraft_publish_is_reported_as_a_landing_issue() {
        let report = PublishReport {
            results: vec![result_with(
                "snapcraft",
                PublisherOutcome::Failed("store rejected upload: dedup collision".into()),
                snapcraft_extra(&[("app", "1.0.0", Some("stable"), false)]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let mut issues = Vec::new();
        let probed = run_landing_checks(&ctx, &log, &panicking_probes(), &mut issues);
        assert_eq!(probed, 0);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("snapcraft"));
        assert!(issues[0].contains("dedup collision"));
    }

    /// Flip a result to advisory (`required: false`).
    fn optional(mut result: PublisherResult) -> PublisherResult {
        result.required = false;
        result
    }

    #[test]
    fn optional_publisher_failure_warns_but_never_fails_the_gate() {
        // The v0.21.0 regression: snapcraft is advisory (required: false) and
        // failed a Snap Store content-dedup. verify-release must NOT turn that
        // into a gate-failing issue — a fatal verdict here skips the downstream
        // OIDC leg and strands the REQUIRED registries (crates.io / npm / PyPI).
        let report = PublishReport {
            results: vec![optional(result_with(
                "snapcraft",
                PublisherOutcome::Failed(
                    "store rejected upload: content-identical dedup at another version".into(),
                ),
                snapcraft_extra(&[("app", "1.0.0", Some("stable"), false)]),
            ))],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let mut issues = Vec::new();
        let probed = run_landing_checks(&ctx, &log, &panicking_probes(), &mut issues);
        assert_eq!(probed, 0);
        assert!(
            issues.is_empty(),
            "an optional publisher's failure must never fail the gate: {issues:?}"
        );
    }

    #[test]
    fn required_publisher_failure_still_fails_the_gate() {
        // The `required` knob is the single source of truth: a required
        // publisher's failure stays fatal even after optional ones are relaxed.
        let report = PublishReport {
            results: vec![result_with(
                "cargo",
                PublisherOutcome::Failed("registry 500".into()),
                cargo_extra(&[("app", "1.0.0")]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &panicking_probes(), &mut issues);
        assert_eq!(issues.len(), 1, "required failure must remain fatal");
        assert!(issues[0].contains("cargo"));
    }

    #[test]
    fn optional_publisher_unverifiable_landing_warns_but_never_fails_the_gate() {
        // A required: false publisher that reported success but whose landing
        // cannot be confirmed (probe says absent) is a false-success — still
        // surfaced (warned) but never gate-failing, since the operator marked
        // it advisory. Required publishers keep failing on the same condition.
        let report = PublishReport {
            results: vec![optional(result_with(
                "snapcraft",
                PublisherOutcome::Succeeded,
                snapcraft_extra(&[("app", "1.0.0", Some("stable"), false)]),
            ))],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let probes = LandingProbes {
            propagation: PropagationRetry::IMMEDIATE,
            cargo_index: &|n, v| panic!("cargo probe must not fire for {n}@{v}"),
            npm_registry: &|_, p, v| panic!("npm probe must not fire for {p}@{v}"),
            pypi_index: &|_, f| panic!("pypi probe must not fire for {f}"),
            blob_head: &|t| panic!("blob probe must not fire for {}", t.key),
            snap_channel_map: &|_, _, _| Ok(false),
            docker_manifest: &|i| panic!("docker probe must not fire for {i}"),
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert!(
            issues.is_empty(),
            "an optional publisher's unverifiable landing must not fail the gate: {issues:?}"
        );
    }

    #[test]
    fn attempted_and_failed_homebrew_publisher_is_reported_as_a_landing_issue() {
        // homebrew has no independently-probeable landing surface (no cargo
        // index / npm registry / bucket / Snap Store equivalent), but a
        // publisher this run actually attempted and failed is a landing
        // defect on its own merits regardless of whether it's one of the
        // six network-probed publishers — the name-list must not gate
        // whether a failure gets reported.
        let report = PublishReport {
            results: vec![result_with(
                "homebrew",
                PublisherOutcome::Failed("tap push rejected: formula conflict".into()),
                PublishEvidenceExtra::Empty,
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let mut issues = Vec::new();
        let probed = run_landing_checks(&ctx, &log, &panicking_probes(), &mut issues);
        assert_eq!(
            probed, 0,
            "homebrew has no landing probe, only the issue report"
        );
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("homebrew"));
        assert!(issues[0].contains("formula conflict"));
    }

    #[test]
    fn config_skipped_snapcraft_is_not_a_landing_issue() {
        let report = PublishReport {
            results: vec![result_with(
                "snapcraft",
                PublisherOutcome::Skipped(SkipReason::NotConfigured),
                snapcraft_extra(&[]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let mut issues = Vec::new();
        let probed = run_landing_checks(&ctx, &log, &panicking_probes(), &mut issues);
        assert_eq!(probed, 0);
        assert!(issues.is_empty());
    }

    #[test]
    fn cargo_visible_versions_produce_no_issues() {
        let report = PublishReport {
            results: vec![result_with(
                "cargo",
                PublisherOutcome::Succeeded,
                cargo_extra(&[("app", "1.0.0"), ("app-core", "1.0.0")]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let calls = Cell::new(0usize);
        let cargo = |_: &str, _: &str| -> anyhow::Result<bool> {
            calls.set(calls.get() + 1);
            Ok(true)
        };
        let probes = LandingProbes {
            cargo_index: &cargo,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        let probed = run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert_eq!(probed, 1, "one publisher probed");
        assert_eq!(calls.get(), 2, "every published crate probed");
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn cargo_missing_version_is_an_issue_naming_the_crate() {
        let report = PublishReport {
            results: vec![result_with(
                "cargo",
                PublisherOutcome::Succeeded,
                cargo_extra(&[("app", "1.0.0"), ("app-core", "1.0.0")]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let cargo = |name: &str, _: &str| -> anyhow::Result<bool> { Ok(name != "app-core") };
        let probes = LandingProbes {
            cargo_index: &cargo,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].contains("app-core@1.0.0") && issues[0].contains("not visible"),
            "{issues:?}"
        );
    }

    #[test]
    fn cargo_probe_error_is_an_issue_not_a_silent_pass() {
        let report = PublishReport {
            results: vec![result_with(
                "cargo",
                PublisherOutcome::Succeeded,
                cargo_extra(&[("app", "1.0.0")]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let cargo =
            |_: &str, _: &str| -> anyhow::Result<bool> { anyhow::bail!("index unreachable") };
        let probes = LandingProbes {
            cargo_index: &cargo,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].contains("could not probe") && issues[0].contains("index unreachable"),
            "{issues:?}"
        );
    }

    #[test]
    fn cargo_custom_registry_targets_are_skipped_not_probed() {
        let extra = PublishEvidenceExtra::Cargo(CargoExtra {
            cargo_yank_targets: vec![CargoYankTargetSnapshot {
                name: "app".to_string(),
                version: "1.0.0".to_string(),
                registry: Some("my-registry".to_string()),
                index: None,
            }],
        });
        let report = PublishReport {
            results: vec![result_with("cargo", PublisherOutcome::Succeeded, extra)],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let mut issues = Vec::new();
        // Panicking cargo probe proves the custom-registry target never
        // reaches the crates.io index probe.
        let probed = run_landing_checks(&ctx, &log, &panicking_probes(), &mut issues);
        assert_eq!(probed, 0, "no crates.io-scoped target => nothing probed");
        assert!(issues.is_empty());
    }

    #[test]
    fn npm_invisible_version_is_an_issue() {
        let report = PublishReport {
            results: vec![result_with(
                "npm",
                PublisherOutcome::Succeeded,
                npm_extra(&[("@scope/app", "1.0.0")]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let npm = |_: &str, _: &str, _: &str| Ok(false);
        let probes = LandingProbes {
            npm_registry: &npm,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        let probed = run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert_eq!(probed, 1);
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].contains("@scope/app@1.0.0")
                && issues[0].contains("registry.npmjs.org")
                && issues[0].contains("is not visible"),
            "a definitive 404 must read as 'not visible': {issues:?}"
        );
    }

    #[test]
    fn npm_indeterminate_probe_is_a_distinct_issue_not_not_visible() {
        // A registry that could not be consulted (5xx/transport) must fail
        // closed as "could not confirm", never as "not visible" — an npm
        // version is immutable, so the latter would fail an already published
        // release on a transient outage.
        let report = PublishReport {
            results: vec![result_with(
                "npm",
                PublisherOutcome::Succeeded,
                npm_extra(&[("app", "3.0.0")]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let npm =
            |_: &str, _: &str, _: &str| Err(anyhow::anyhow!("502 Bad Gateway: registry down"));
        let probes = LandingProbes {
            npm_registry: &npm,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        let probed = run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert_eq!(probed, 1);
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].contains("could not confirm app@3.0.0")
                && !issues[0].contains("is not visible"),
            "indeterminate must be its own issue, not a false 'not visible': {issues:?}"
        );
    }

    #[test]
    fn npm_visible_version_passes_with_recorded_coordinates() {
        let report = PublishReport {
            results: vec![result_with(
                "npm",
                PublisherOutcome::Succeeded,
                npm_extra(&[("app", "2.0.0")]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let seen = Cell::new(false);
        let npm = |registry: &str, package: &str, version: &str| {
            assert_eq!(registry, "https://registry.npmjs.org");
            assert_eq!(package, "app");
            assert_eq!(version, "2.0.0");
            seen.set(true);
            Ok(true)
        };
        let probes = LandingProbes {
            npm_registry: &npm,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert!(seen.get(), "probe must receive the recorded coordinates");
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn blob_missing_and_unverifiable_objects_are_distinct_issues() {
        let report = PublishReport {
            results: vec![result_with(
                "blob",
                PublisherOutcome::Succeeded,
                blob_extra(&["v1/app.tar.gz", "v1/checksums.txt", "v1/app.sig"]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let blob = |t: &BlobTargetSnapshot| -> anyhow::Result<bool> {
            match t.key.as_str() {
                "v1/app.tar.gz" => Ok(true),
                "v1/checksums.txt" => Ok(false),
                _ => anyhow::bail!("HEAD timed out"),
            }
        };
        let probes = LandingProbes {
            blob_head: &blob,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        let probed = run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert_eq!(probed, 1);
        assert_eq!(issues.len(), 2);
        assert!(
            issues
                .iter()
                .any(|i| i.contains("s3://bkt/v1/checksums.txt") && i.contains("missing")),
            "{issues:?}"
        );
        assert!(
            issues
                .iter()
                .any(|i| i.contains("s3://bkt/v1/app.sig") && i.contains("could not verify")),
            "{issues:?}"
        );
    }

    fn snapcraft_extra(targets: &[(&str, &str, Option<&str>, bool)]) -> PublishEvidenceExtra {
        PublishEvidenceExtra::Snapcraft(anodizer_core::publish_evidence::SnapcraftExtra {
            snapcraft_targets: targets
                .iter()
                .map(|(name, version, channel, held)| SnapcraftTargetSnapshot {
                    crate_name: name.to_string(),
                    package_name: name.to_string(),
                    channel: channel.map(|c| c.to_string()),
                    arch: None,
                    revision: None,
                    version: Some(version.to_string()),
                    held_for_review: *held,
                })
                .collect(),
        })
    }

    #[test]
    fn snapcraft_visible_version_passes_with_recorded_coordinates() {
        let report = PublishReport {
            results: vec![result_with(
                "snapcraft",
                PublisherOutcome::Succeeded,
                snapcraft_extra(&[("demo", "1.2.3", Some("stable"), false)]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let seen = Cell::new(false);
        let snap = |name: &str, version: &str, channel: Option<&str>| {
            assert_eq!(name, "demo");
            assert_eq!(version, "1.2.3");
            assert_eq!(channel, Some("stable"));
            seen.set(true);
            Ok(true)
        };
        let probes = LandingProbes {
            snap_channel_map: &snap,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        let probed = run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert_eq!(probed, 1);
        assert!(seen.get(), "probe must receive the recorded coordinates");
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn snapcraft_held_and_absent_version_reports_the_review_hold() {
        let report = PublishReport {
            results: vec![result_with(
                "snapcraft",
                PublisherOutcome::Succeeded,
                snapcraft_extra(&[("demo", "1.2.3", Some("stable"), true)]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let snap = |_: &str, _: &str, _: Option<&str>| Ok(false);
        let probes = LandingProbes {
            snap_channel_map: &snap,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].contains("demo 1.2.3")
                && issues[0].contains("HELD for Snap Store manual review")
                && issues[0].contains("dashboard.snapcraft.io/snaps/demo/"),
            "a held-and-absent snap must name the review hold: {issues:?}"
        );
    }

    #[test]
    fn snapcraft_absent_version_without_hold_reads_as_not_in_channel_map() {
        let report = PublishReport {
            results: vec![result_with(
                "snapcraft",
                PublisherOutcome::Succeeded,
                snapcraft_extra(&[("demo", "2.0.0", Some("stable"), false)]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let snap = |_: &str, _: &str, _: Option<&str>| Ok(false);
        let probes = LandingProbes {
            snap_channel_map: &snap,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].contains("not in the store's channel map for channel 'stable'")
                && !issues[0].contains("HELD"),
            "{issues:?}"
        );
    }

    #[test]
    fn snapcraft_probe_error_is_an_issue_not_a_silent_pass() {
        let report = PublishReport {
            results: vec![result_with(
                "snapcraft",
                PublisherOutcome::Succeeded,
                snapcraft_extra(&[("demo", "1.0.0", None, false)]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let snap = |_: &str, _: &str, _: Option<&str>| anyhow::bail!("store unreachable");
        let probes = LandingProbes {
            snap_channel_map: &snap,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].contains("could not probe") && issues[0].contains("store unreachable"),
            "{issues:?}"
        );
    }

    #[test]
    fn snapcraft_versionless_snapshot_is_skipped_not_probed() {
        // A pre-`version` snapshot has nothing honest to probe — the
        // panicking probe proves it is never consulted.
        let extra =
            PublishEvidenceExtra::Snapcraft(anodizer_core::publish_evidence::SnapcraftExtra {
                snapcraft_targets: vec![SnapcraftTargetSnapshot {
                    crate_name: "demo".to_string(),
                    package_name: "demo".to_string(),
                    channel: Some("stable".to_string()),
                    arch: None,
                    revision: None,
                    version: None,
                    held_for_review: false,
                }],
            });
        let report = PublishReport {
            results: vec![result_with("snapcraft", PublisherOutcome::Succeeded, extra)],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let mut issues = Vec::new();
        let probed = run_landing_checks(&ctx, &log, &panicking_probes(), &mut issues);
        assert_eq!(probed, 0, "no version recorded => nothing probed");
        assert!(issues.is_empty());
    }

    #[test]
    fn unrelated_succeeded_publishers_are_ignored() {
        let report = PublishReport {
            results: vec![result_with(
                "homebrew",
                PublisherOutcome::Succeeded,
                PublishEvidenceExtra::Empty,
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let mut issues = Vec::new();
        let probed = run_landing_checks(&ctx, &log, &panicking_probes(), &mut issues);
        assert_eq!(probed, 0);
        assert!(issues.is_empty());
    }

    #[test]
    fn the_propagation_window_never_outlives_the_run_deadline() {
        use std::time::{Duration, Instant};
        let retry = PropagationRetry::DEFAULT;

        let anchored = Instant::now();
        let open = retry.starting_now(None).sweep_deadline.expect("anchored");
        assert!(open >= anchored + Duration::from_secs(179), "{open:?}");
        assert!(
            open <= Instant::now() + Duration::from_secs(180),
            "{open:?}"
        );

        let far = Instant::now() + Duration::from_secs(3600);
        let capped = retry
            .starting_now(Some(far))
            .sweep_deadline
            .expect("anchored");
        assert!(
            capped < far,
            "the run deadline is far, so the window governs"
        );

        let near = Instant::now() + Duration::from_secs(10);
        assert_eq!(
            retry.starting_now(Some(near)).sweep_deadline,
            Some(near),
            "a run deadline inside the window governs instead"
        );

        let passed = Instant::now() - Duration::from_secs(1);
        assert_eq!(
            retry.starting_now(Some(passed)).sweep_deadline,
            Some(passed),
            "an already-spent run budget leaves no window at all"
        );
    }

    /// The window belongs to the sweep, not to one target: probing many
    /// never-landing targets must cost ONE window, not one per target.
    #[test]
    fn a_sweep_over_many_targets_closes_within_one_window() {
        use std::time::{Duration, Instant};
        let packages: Vec<(&str, &str)> = vec![
            ("a", "1.0.0"),
            ("b", "1.0.0"),
            ("c", "1.0.0"),
            ("d", "1.0.0"),
            ("e", "1.0.0"),
            ("f", "1.0.0"),
        ];
        let report = PublishReport {
            results: vec![result_with(
                "npm",
                PublisherOutcome::Succeeded,
                npm_extra(&packages),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let window = Duration::from_millis(120);
        let propagation = PropagationRetry {
            policy: anodizer_core::retry::RetryPolicy {
                max_attempts: 100,
                base_delay: Duration::from_millis(20),
                max_delay: Duration::from_millis(20),
            },
            budget: window,
            ..PropagationRetry::DEFAULT
        }
        .starting_now(None);
        let npm = |_: &str, _: &str, _: &str| Ok(false);
        let probes = LandingProbes {
            propagation,
            npm_registry: &npm,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        let started = Instant::now();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        let spent = started.elapsed();
        assert_eq!(issues.len(), packages.len(), "{issues:?}");
        assert!(
            spent < window * 3,
            "six never-landing targets shared one {window:?} window but spent {spent:?}"
        );
    }

    /// The ladder stops at the window's edge, and stops early when the
    /// remaining window is shorter than the next backoff.
    #[test]
    fn the_ladder_stops_at_the_sweep_deadline() {
        use std::time::{Duration, Instant};
        let retry = PropagationRetry {
            policy: anodizer_core::retry::RetryPolicy {
                max_attempts: 100,
                base_delay: Duration::from_millis(10),
                max_delay: Duration::from_millis(10),
            },
            budget: Duration::from_millis(25),
            ..PropagationRetry::DEFAULT
        }
        .starting_now(None);
        let ctx = Context::new(Config::default(), ContextOptions::default());
        let log = test_logger(&ctx);
        let asks = Cell::new(0usize);
        let started = Instant::now();
        let outcome = probe_with_propagation("t", "nowhere", &retry, &log, || {
            asks.set(asks.get() + 1);
            Ok(false)
        });
        let spent = started.elapsed();
        assert!(matches!(outcome.landed, Landed::No));
        assert!(outcome.waited, "more than one ask happened");
        // 25ms of window at a flat 10ms backoff: two sleeps fit, the third
        // would fall past the deadline, so the ladder stops with the
        // remaining window shorter than one base delay. A loaded runner can
        // overrun one sleep and fit only one, so the lower bound is two;
        // the upper bound is what holds the property.
        let asked = asks.get();
        assert!(
            (2..=3).contains(&asked),
            "the ladder stops at the deadline: asked {asked} times"
        );
        assert!(
            spent < Duration::from_millis(500),
            "the ladder must not outlive its window: {spent:?}"
        );
    }

    /// A probe failure that re-asking cannot resolve — a rejected credential,
    /// a store that could not be built — must end THAT target immediately
    /// instead of spending the sweep's shared window on a fixed answer.
    #[test]
    fn a_non_retriable_probe_failure_breaks_without_re_asking() {
        /// Drive every probe of one run with a 403 and assert the ladder
        /// asked exactly once. `finding` reads the recorded finding back out
        /// of the run: a gating publisher records an issue, docker's
        /// unverifiable landing a warning.
        fn assert_one_ask(
            publisher: &str,
            ctx: &Context,
            finding: impl Fn(&[String]) -> Vec<String>,
        ) {
            let log = test_logger(ctx);
            let asks = Cell::new(0usize);
            let deny = || -> anyhow::Result<bool> {
                asks.set(asks.get() + 1);
                Err(anyhow::Error::new(anodizer_core::retry::HttpError::new(
                    std::io::Error::other("403 Forbidden: token lacks publish scope"),
                    403,
                )))
            };
            let probes = LandingProbes {
                propagation: FLAKY,
                cargo_index: &|_, _| deny(),
                npm_registry: &|_, _, _| deny(),
                pypi_index: &|_, _| deny(),
                blob_head: &|_| deny(),
                snap_channel_map: &|_, _, _| deny(),
                docker_manifest: &|_| deny().map(|_| None),
            };
            let mut issues = Vec::new();
            run_landing_checks(ctx, &log, &probes, &mut issues);
            assert_eq!(
                asks.get(),
                1,
                "{publisher}: a 403 answers the same on every re-ask"
            );
            let findings = finding(&issues);
            assert_eq!(findings.len(), 1, "{publisher}: {findings:?}");
            assert!(
                findings[0].contains("403 Forbidden"),
                "{publisher}: {findings:?}"
            );
        }

        for (publisher, extra) in [
            ("cargo", cargo_extra(&[("app", "1.0.0")])),
            ("npm", npm_extra(&[("app", "1.0.0")])),
            ("pypi", pypi_extra(&["app-1.0.0-py3-none-any.whl"])),
            ("blob", blob_extra(&["v1/app.tar.gz"])),
            (
                "snapcraft",
                snapcraft_extra(&[("app", "1.0.0", Some("stable"), false)]),
            ),
        ] {
            let report = PublishReport {
                results: vec![result_with(publisher, PublisherOutcome::Succeeded, extra)],
                ..Default::default()
            };
            assert_one_ask(publisher, &ctx_with_report(report), |issues| {
                issues.to_vec()
            });
        }
        // Docker files no publish report, so its one target comes from the
        // artifacts instead, and an unverifiable landing is a warning.
        let (ctx, capture) = ctx_with_pushed_images(&[("ghcr.io/owner/app:1.0.0", None, true)]);
        assert_one_ask("docker", &ctx, |issues| {
            assert!(issues.is_empty(), "{issues:?}");
            capture.warn_messages()
        });
    }

    fn pypi_extra(filenames: &[&str]) -> PublishEvidenceExtra {
        PublishEvidenceExtra::Pypi(anodizer_core::publish_evidence::PypiExtra {
            pypi_files: filenames
                .iter()
                .map(|f| PypiFileSnapshot {
                    filename: f.to_string(),
                    platform_tag: "any".to_string(),
                    sha256: "0".repeat(64),
                    repository: "https://upload.pypi.org/legacy/".to_string(),
                    skipped_existing: false,
                })
                .collect(),
        })
    }

    #[test]
    fn pypi_listed_files_pass_with_recorded_coordinates() {
        let report = PublishReport {
            results: vec![result_with(
                "pypi",
                PublisherOutcome::Succeeded,
                pypi_extra(&[
                    "app-1.0.0-py3-none-manylinux_2_28_x86_64.whl",
                    "app-1.0.0.tar.gz",
                ]),
            )],
            ..Default::default()
        };
        let (ctx, capture) = ctx_capturing(report);
        let log = test_logger(&ctx);
        let seen = std::cell::RefCell::new(Vec::new());
        let pypi = |repository: &str, filename: &str| {
            assert_eq!(repository, "https://upload.pypi.org/legacy/");
            seen.borrow_mut().push(filename.to_string());
            Ok(true)
        };
        let probes = LandingProbes {
            pypi_index: &pypi,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        let probed = run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert_eq!(probed, 1);
        assert_eq!(
            *seen.borrow(),
            vec![
                "app-1.0.0-py3-none-manylinux_2_28_x86_64.whl".to_string(),
                "app-1.0.0.tar.gz".to_string()
            ],
            "every recorded file is probed by its own name"
        );
        assert!(issues.is_empty(), "{issues:?}");
        assert!(
            statuses(&capture)
                .iter()
                .any(|m| m == "pypi: 2/2 uploaded file(s) listed on pypi.org"),
            "{:?}",
            statuses(&capture)
        );
    }

    /// The six result lines are one grammar: a singular form for one target,
    /// and every host named when several were asked. npm and blob carry a
    /// per-target host, so both must sort and dedup the list they print.
    #[test]
    fn the_npm_result_line_is_singular_for_one_package_and_names_every_registry() {
        let one = PublishReport {
            results: vec![result_with(
                "npm",
                PublisherOutcome::Succeeded,
                npm_extra(&[("app", "1.0.0")]),
            )],
            ..Default::default()
        };
        let (ctx, capture) = ctx_capturing(one);
        let log = test_logger(&ctx);
        let npm = |_: &str, _: &str, _: &str| Ok(true);
        let probes = LandingProbes {
            npm_registry: &npm,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert!(
            statuses(&capture)
                .iter()
                .any(|m| m == "npm: app@1.0.0 visible on registry.npmjs.org"),
            "{:?}",
            statuses(&capture)
        );

        let mut two = npm_extra(&[("app", "1.0.0"), ("app-linux-x64", "1.0.0")]);
        if let PublishEvidenceExtra::Npm(extra) = &mut two {
            extra.npm_targets[1].registry = "https://npm.pkg.github.com".to_string();
        }
        let report = PublishReport {
            results: vec![result_with("npm", PublisherOutcome::Succeeded, two)],
            ..Default::default()
        };
        let (ctx, capture) = ctx_capturing(report);
        let log = test_logger(&ctx);
        let probes = LandingProbes {
            npm_registry: &npm,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert!(
            statuses(&capture).iter().any(|m| m
                == "npm: 2/2 published package(s) visible on npm.pkg.github.com, \
                    registry.npmjs.org"),
            "{:?}",
            statuses(&capture)
        );
    }

    /// blob was the only publisher with no singular branch, and it named no
    /// bucket at all. Its line now reads like its five siblings: one subject,
    /// one verb, every host it reached.
    #[test]
    fn the_blob_result_line_is_singular_for_one_object_and_names_every_bucket() {
        let one = PublishReport {
            results: vec![result_with(
                "blob",
                PublisherOutcome::Succeeded,
                blob_extra(&["v1/app.tar.gz"]),
            )],
            ..Default::default()
        };
        let (ctx, capture) = ctx_capturing(one);
        let log = test_logger(&ctx);
        let head = |_: &BlobTargetSnapshot| Ok(true);
        let probes = LandingProbes {
            blob_head: &head,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert!(
            statuses(&capture)
                .iter()
                .any(|m| m == "blob: v1/app.tar.gz visible in s3://bkt"),
            "{:?}",
            statuses(&capture)
        );

        let mut two = blob_extra(&["v1/app.tar.gz", "v1/app.zip"]);
        if let PublishEvidenceExtra::Blob(extra) = &mut two {
            extra.blob_targets[1].bucket = "mirror".to_string();
        }
        let report = PublishReport {
            results: vec![result_with("blob", PublisherOutcome::Succeeded, two)],
            ..Default::default()
        };
        let (ctx, capture) = ctx_capturing(report);
        let log = test_logger(&ctx);
        let probes = LandingProbes {
            blob_head: &head,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert!(
            statuses(&capture)
                .iter()
                .any(|m| m == "blob: 2/2 uploaded object(s) visible in s3://bkt, s3://mirror"),
            "{:?}",
            statuses(&capture)
        );
    }

    /// Every landing result line follows one grammar. Asked of the check
    /// functions themselves so the seventh probe cannot invent a third shape.
    #[test]
    fn every_landing_result_line_has_a_singular_branch_and_dedups_its_hosts() {
        use anodizer_core::test_helpers::test_sources::{function_bodies, production_half};

        let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/landing.rs"))
            .expect("read landing.rs");
        let mut checked = Vec::new();
        for body in function_bodies(production_half(&text)) {
            let signature = body.trim_start().lines().next().unwrap_or_default();
            if !signature.contains("fn check_") || !signature.contains("_landing") {
                continue;
            }
            checked.push(signature.to_string());
            assert!(
                body.contains("log.status("),
                "a landing check reports nothing: {signature}"
            );
            assert!(
                body.contains("== 1"),
                "a result line with no singular branch reads `1/1 …` for one \
                 target: {signature}"
            );
            // A line whose host comes from a PER-TARGET field must name every
            // one of them, sorted and deduped: naming only the first credits
            // one registry's artifacts to another, and an unsorted list
            // reorders between runs. cargo (the crates.io index) and
            // snapcraft (the Snap Store) name a constant host and stay out.
            let per_target_host = body.contains("registry_host(&t")
                || body.contains("index_host(&t")
                || body.contains("image_registry(&i")
                || body.contains(".bucket");
            if per_target_host {
                assert!(
                    body.contains(r#".join(", ")"#)
                        && body.contains(".sort();")
                        && body.contains(".dedup();"),
                    "a per-target host list must be joined, sorted and \
                     deduped: {signature}"
                );
            }
        }
        assert_eq!(
            checked.len(),
            6,
            "one check function per landing probe: {checked:?}"
        );
    }

    /// One image the probe could not reach is a warning, not a finding, so the
    /// run passes — and the images that DID verify are still reported.
    #[test]
    fn an_unverifiable_image_does_not_silence_the_verified_ones() {
        let (ctx, capture) = ctx_with_pushed_images(&[
            ("ghcr.io/owner/app:1.0.0", None, true),
            ("private.example.com/owner/app:1.0.0", None, true),
        ]);
        let log = test_logger(&ctx);
        let manifest = |reference: &str| {
            if reference.starts_with("ghcr.io") {
                Ok(Some("sha256:aa".to_string()))
            } else {
                Err(anyhow::anyhow!("401 Unauthorized"))
            }
        };
        let probes = LandingProbes {
            docker_manifest: &manifest,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert!(issues.is_empty(), "{issues:?}");
        assert!(
            statuses(&capture).iter().any(|m| m
                == "docker: 1/2 pushed image(s) visible on ghcr.io, \
                    private.example.com (1 unverifiable)"),
            "{:?}",
            statuses(&capture)
        );
    }

    /// A run that both waited for propagation and could not reach one
    /// registry says so in ONE trailing clause — two parentheticals in a row
    /// read as two separate results.
    #[test]
    fn a_waited_docker_run_with_an_unverifiable_image_prints_one_trailing_clause() {
        let (ctx, capture) = ctx_with_pushed_images(&[
            ("ghcr.io/owner/app:1.0.0", None, true),
            ("private.example.com/owner/app:1.0.0", None, true),
        ]);
        let log = test_logger(&ctx);
        let asks = std::cell::Cell::new(0usize);
        let manifest = |reference: &str| {
            if reference.starts_with("ghcr.io") {
                asks.set(asks.get() + 1);
                // Absent on the first ask, served on the second: the shape of
                // a registry that has accepted a push but not yet serves it.
                Ok((asks.get() > 1).then(|| "sha256:aa".to_string()))
            } else {
                Err(anyhow::anyhow!("401 Unauthorized"))
            }
        };
        let probes = LandingProbes {
            propagation: PropagationRetry::immediate_attempts(3),
            docker_manifest: &manifest,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert!(issues.is_empty(), "{issues:?}");
        assert!(
            statuses(&capture).iter().any(|m| m
                == "docker: 1/2 pushed image(s) visible on ghcr.io, \
                    private.example.com (1/2 needed a propagation wait, 1 \
                    unverifiable)"),
            "{:?}",
            statuses(&capture)
        );
    }

    /// One uploaded file reads as itself, not as `1/1 uploaded file(s)`, and a
    /// run that uploaded to two indexes names both rather than crediting every
    /// file to the first.
    #[test]
    fn the_pypi_result_line_is_singular_for_one_file_and_names_every_index() {
        let one = PublishReport {
            results: vec![result_with(
                "pypi",
                PublisherOutcome::Succeeded,
                pypi_extra(&["app-1.0.0.tar.gz"]),
            )],
            ..Default::default()
        };
        let (ctx, capture) = ctx_capturing(one);
        let log = test_logger(&ctx);
        let pypi = |_: &str, _: &str| Ok(true);
        let probes = LandingProbes {
            pypi_index: &pypi,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert!(
            statuses(&capture)
                .iter()
                .any(|m| m == "pypi: app-1.0.0.tar.gz listed on pypi.org"),
            "{:?}",
            statuses(&capture)
        );

        let mut two = pypi_extra(&["app-1.0.0.tar.gz", "app-1.0.0-py3-none-any.whl"]);
        if let PublishEvidenceExtra::Pypi(extra) = &mut two {
            extra.pypi_files[1].repository = "https://test.pypi.org/legacy/".to_string();
        }
        let report = PublishReport {
            results: vec![result_with("pypi", PublisherOutcome::Succeeded, two)],
            ..Default::default()
        };
        let (ctx, capture) = ctx_capturing(report);
        let log = test_logger(&ctx);
        let probes = LandingProbes {
            pypi_index: &pypi,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert!(
            statuses(&capture)
                .iter()
                .any(|m| m == "pypi: 2/2 uploaded file(s) listed on pypi.org, test.pypi.org"),
            "{:?}",
            statuses(&capture)
        );
    }

    #[test]
    fn pypi_unlisted_file_is_an_issue_naming_the_filename() {
        let report = PublishReport {
            results: vec![result_with(
                "pypi",
                PublisherOutcome::Succeeded,
                pypi_extra(&["app-1.0.0-py3-none-win_amd64.whl", "app-1.0.0.tar.gz"]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let pypi = |_: &str, filename: &str| Ok(filename.ends_with(".tar.gz"));
        let probes = LandingProbes {
            pypi_index: &pypi,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].contains("app-1.0.0-py3-none-win_amd64.whl")
                && issues[0].contains("is not listed on pypi.org"),
            "a partial upload names the missing wheel: {issues:?}"
        );
    }

    #[test]
    fn pypi_indeterminate_probe_is_a_distinct_issue_not_unlisted() {
        // A PyPI filename is a permanent index slot that can never be
        // re-uploaded, so an index that could not be consulted must never be
        // reported as an absence.
        let report = PublishReport {
            results: vec![result_with(
                "pypi",
                PublisherOutcome::Succeeded,
                pypi_extra(&["app-1.0.0.tar.gz"]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let pypi = |_: &str, _: &str| anyhow::bail!("502 Bad Gateway: index down");
        let probes = LandingProbes {
            propagation: FLAKY,
            pypi_index: &pypi,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].contains("could not confirm app-1.0.0.tar.gz")
                && !issues[0].contains("is not listed"),
            "{issues:?}"
        );
    }

    #[test]
    fn pypi_probe_retries_a_propagating_index() {
        let report = PublishReport {
            results: vec![result_with(
                "pypi",
                PublisherOutcome::Succeeded,
                pypi_extra(&["app-1.0.0.tar.gz"]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let answers = scripted(vec![Some(false), Some(false), Some(true)]);
        let pypi = |_: &str, _: &str| answers();
        let probes = LandingProbes {
            propagation: FLAKY,
            pypi_index: &pypi,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn pypi_probe_error_then_success_is_not_an_issue() {
        let report = PublishReport {
            results: vec![result_with(
                "pypi",
                PublisherOutcome::Succeeded,
                pypi_extra(&["app-1.0.0.tar.gz"]),
            )],
            ..Default::default()
        };
        let ctx = ctx_with_report(report);
        let log = test_logger(&ctx);
        let answers = scripted(vec![None, Some(true)]);
        let pypi = |_: &str, _: &str| answers();
        let probes = LandingProbes {
            propagation: FLAKY,
            pypi_index: &pypi,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert!(issues.is_empty(), "{issues:?}");
    }

    /// A propagation wait is the expected case, so nothing about it may reach
    /// default-verbosity output as a warning; under `-v` the ladder's own
    /// per-attempt lines come back.
    #[test]
    fn the_propagation_ladder_is_silent_at_default_and_speaks_under_verbose() {
        let report = || PublishReport {
            results: vec![result_with(
                "npm",
                PublisherOutcome::Succeeded,
                npm_extra(&[("demo", "1.0.0")]),
            )],
            ..Default::default()
        };

        let (ctx, quiet) = ctx_capturing(report());
        let answers = scripted(vec![Some(false), Some(true)]);
        let npm = |_: &str, _: &str, _: &str| answers();
        let probes = LandingProbes {
            propagation: FLAKY,
            npm_registry: &npm,
            ..panicking_probes()
        };
        run_landing_checks(&ctx, &test_logger(&ctx), &probes, &mut Vec::new());
        assert_eq!(
            quiet.warn_count(),
            0,
            "default verbosity must carry no warning: {:?}",
            quiet.warn_messages()
        );

        let capture = anodizer_core::log::LogCapture::new();
        let mut verbose = Context::new(
            Config::default(),
            ContextOptions {
                verbose: true,
                ..Default::default()
            },
        );
        verbose.set_publish_report(report());
        verbose.with_log_capture(capture.clone());
        let answers = scripted(vec![Some(false), Some(true)]);
        let npm = |_: &str, _: &str, _: &str| answers();
        let probes = LandingProbes {
            propagation: FLAKY,
            npm_registry: &npm,
            ..panicking_probes()
        };
        run_landing_checks(&verbose, &test_logger(&verbose), &probes, &mut Vec::new());
        assert!(
            capture
                .warn_messages()
                .iter()
                .any(|m| m.contains("npm: demo@1.0.0 landing probe on registry.npmjs.org")),
            "under -v the ladder names each attempt: {:?}",
            capture.warn_messages()
        );
    }

    /// Every probe on [`LandingProbes`] is asked through
    /// [`probe_with_propagation`] — a probe called directly would get its own
    /// window (or none) and drift from the sweep's single bound.
    /// See `.claude/rules/landing-probes-propagation.md`.
    /// A registry that has accepted a push does not always serve the tag on
    /// the next request — the same propagation the npm and crates.io probes
    /// wait out.
    #[test]
    fn docker_probe_retries_until_the_registry_serves_the_tag() {
        let (ctx, capture) = ctx_with_pushed_images(&[("ghcr.io/owner/app:1.0.0", None, true)]);
        let log = test_logger(&ctx);
        let asks = Cell::new(0usize);
        let docker = |_: &str| -> anyhow::Result<Option<String>> {
            asks.set(asks.get() + 1);
            Ok((asks.get() >= 3).then(|| "sha256:aaa".to_string()))
        };
        let probes = LandingProbes {
            propagation: FLAKY,
            docker_manifest: &docker,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        let probed = run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(probed, 1);
        assert_eq!(
            capture.warn_count(),
            0,
            "waiting out propagation is not a defect: {:?}",
            capture.warn_messages()
        );
        let lines = statuses(&capture);
        assert!(
            lines.iter().any(|m| m.starts_with(
                "docker: ghcr.io/owner/app:1.0.0 visible on ghcr.io (1/1 needed a propagation wait"
            )),
            "the result line carries the propagation count: {lines:?}"
        );
    }

    #[test]
    fn docker_probe_reports_the_absence_once_the_window_closes() {
        let (ctx, _capture) = ctx_with_pushed_images(&[("ghcr.io/owner/app:1.0.0", None, true)]);
        let log = test_logger(&ctx);
        let docker = |_: &str| Ok(None);
        let probes = LandingProbes {
            propagation: FLAKY,
            docker_manifest: &docker,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert_eq!(
            issues,
            vec![
                "docker: ghcr.io/owner/app:1.0.0 reported pushed but is not visible on ghcr.io"
                    .to_string()
            ]
        );
    }

    /// A registry that could not be consulted is unverifiable, not absent: a
    /// pushed tag behind a credential the probe lacks is still live for
    /// everyone holding one. It is recorded as a warning and never fails the
    /// release, the way an optional publisher's landing finding is — a fatal
    /// verdict here would strand the one-way-door publishers downstream.
    #[test]
    fn docker_probe_reports_an_unreachable_registry_as_unverifiable() {
        let (ctx, capture) = ctx_with_pushed_images(&[("ghcr.io/owner/app:1.0.0", None, true)]);
        let log = test_logger(&ctx);
        let docker = |_: &str| -> anyhow::Result<Option<String>> {
            anyhow::bail!("connection reset by peer")
        };
        let probes = LandingProbes {
            propagation: FLAKY,
            docker_manifest: &docker,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert!(
            issues.is_empty(),
            "an unanswerable registry is not a gate: {issues:?}"
        );
        let warnings = capture.warn_messages();
        assert!(
            warnings.iter().any(|m| m.starts_with(
                "unverifiable docker landing not gating the release — docker: could not confirm \
                 ghcr.io/owner/app:1.0.0 on ghcr.io: connection reset"
            )),
            "the finding is still recorded: {warnings:?}"
        );
    }

    /// An operator who told this leg to leave the registry alone gets no
    /// probe, even when the rehydrated manifest still carries the pushed
    /// markers of the leg that did push.
    #[test]
    fn a_deselected_docker_publisher_is_not_probed() {
        for opts in [
            ContextOptions {
                skip_stages: vec!["docker".to_string()],
                ..Default::default()
            },
            ContextOptions {
                publisher_allowlist: vec!["cargo".to_string()],
                ..Default::default()
            },
        ] {
            let (base, _capture) =
                ctx_with_pushed_images(&[("ghcr.io/owner/app:1.0.0", None, true)]);
            let mut ctx = Context::new(Config::default(), opts);
            for artifact in base.artifacts.all() {
                ctx.artifacts.add(artifact.clone());
            }
            let log = test_logger(&ctx);
            let mut issues = Vec::new();
            let probed = run_landing_checks(&ctx, &log, &panicking_probes(), &mut issues);
            assert_eq!(probed, 0, "a deselected publisher is not probed");
            assert!(issues.is_empty(), "{issues:?}");
        }
    }

    /// A tag serving other content answers the same way every time, so the
    /// ladder ends after one re-ask instead of spending the window every
    /// other target of the sweep is queued behind.
    #[test]
    fn a_tag_serving_a_different_digest_stops_after_one_re_ask() {
        let (ctx, _capture) =
            ctx_with_pushed_images(&[("ghcr.io/owner/app:1.0.0", Some("sha256:aaa"), true)]);
        let log = test_logger(&ctx);
        let asks = Cell::new(0usize);
        let docker = |_: &str| -> anyhow::Result<Option<String>> {
            asks.set(asks.get() + 1);
            Ok(Some("sha256:bbb".to_string()))
        };
        let probes = LandingProbes {
            propagation: PropagationRetry::immediate_attempts(8),
            docker_manifest: &docker,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert_eq!(asks.get(), 2, "one ask, one re-ask, then the fixed answer");
        assert_eq!(
            issues,
            vec![
                "docker: ghcr.io/owner/app:1.0.0 was pushed as sha256:aaa but ghcr.io serves \
                 sha256:bbb"
                    .to_string()
            ]
        );
    }

    /// A run can push to several registries, so the counted result line names
    /// every one it asked rather than crediting them all to the first.
    #[test]
    fn the_docker_result_line_names_every_registry_it_probed() {
        let (ctx, capture) = ctx_with_pushed_images(&[
            ("ghcr.io/owner/app:1.0.0", None, true),
            ("registry.example/owner/app:1.0.0", None, true),
        ]);
        let log = test_logger(&ctx);
        let docker = |_: &str| Ok(Some("sha256:aaa".to_string()));
        let probes = LandingProbes {
            propagation: PropagationRetry::IMMEDIATE,
            docker_manifest: &docker,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert!(issues.is_empty(), "{issues:?}");
        let lines = statuses(&capture);
        assert!(
            lines
                .iter()
                .any(|m| m == "docker: 2/2 pushed image(s) visible on ghcr.io, registry.example"),
            "{lines:?}"
        );
    }

    /// An error followed by a success is propagation, not a finding.
    #[test]
    fn a_docker_probe_error_then_success_is_not_an_issue() {
        let (ctx, capture) = ctx_with_pushed_images(&[("ghcr.io/owner/app:1.0.0", None, true)]);
        let log = test_logger(&ctx);
        let asks = Cell::new(0usize);
        let docker = |_: &str| -> anyhow::Result<Option<String>> {
            asks.set(asks.get() + 1);
            if asks.get() == 1 {
                anyhow::bail!("connection reset by peer");
            }
            Ok(Some("sha256:aaa".to_string()))
        };
        let probes = LandingProbes {
            propagation: FLAKY,
            docker_manifest: &docker,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(capture.warn_count(), 0, "{:?}", capture.warn_messages());
    }

    /// A tag that resolves to content the run did not push is a different
    /// defect from an absent tag, and a presence-only question would pass it.
    #[test]
    fn a_tag_serving_a_different_digest_is_reported_as_a_mismatch() {
        let (ctx, _capture) =
            ctx_with_pushed_images(&[("ghcr.io/owner/app:1.0.0", Some("sha256:aaa"), true)]);
        let log = test_logger(&ctx);
        let docker = |_: &str| Ok(Some("sha256:bbb".to_string()));
        let probes = LandingProbes {
            propagation: FLAKY,
            docker_manifest: &docker,
            ..panicking_probes()
        };
        let mut issues = Vec::new();
        run_landing_checks(&ctx, &log, &probes, &mut issues);
        assert_eq!(
            issues,
            vec![
                "docker: ghcr.io/owner/app:1.0.0 was pushed as sha256:aaa but ghcr.io serves \
                 sha256:bbb"
                    .to_string()
            ]
        );
    }

    /// An image the run built but never pushed — a snapshot, a dry run, a
    /// `skip_push:` manifest, or an unpushed image rehydrated from the
    /// preserved manifest — is not a landing target.
    #[test]
    fn an_image_that_was_never_pushed_is_not_probed() {
        let (ctx, _capture) = ctx_with_pushed_images(&[("ghcr.io/owner/app:1.0.0", None, false)]);
        let log = test_logger(&ctx);
        let mut issues = Vec::new();
        let probed = run_landing_checks(&ctx, &log, &panicking_probes(), &mut issues);
        assert_eq!(probed, 0);
        assert!(issues.is_empty(), "{issues:?}");
    }

    /// `PropagationRetry::DEFAULT` carries no anchor, so a caller that hands
    /// it to a sweep unanchored gives every probe an attempt-count ladder with
    /// no shared wall-clock bound.
    #[test]
    fn the_default_propagation_window_is_always_anchored_before_use() {
        use anodizer_core::test_helpers::test_sources::{
            function_bodies, production_half, rust_sources,
        };

        let src = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src"));
        let mut users = 0usize;
        for source in rust_sources(src) {
            let text = std::fs::read_to_string(&source).expect("read source");
            for body in function_bodies(production_half(&text)) {
                // Reading one field off the constant (the no-sleep window
                // borrows its budget) takes no window and needs no anchor.
                let uses = body.replace("PropagationRetry::DEFAULT.budget", "");
                if !uses.contains("PropagationRetry::DEFAULT") {
                    continue;
                }
                users += 1;
                assert!(
                    body.contains("starting_now"),
                    "{}: {} uses PropagationRetry::DEFAULT without anchoring it",
                    source.display(),
                    body.trim_start().lines().next().unwrap_or_default()
                );
            }
        }
        assert_eq!(
            users, 1,
            "the sweep anchors the window once, in VerifyReleaseStage::run"
        );
    }

    #[test]
    fn every_landing_probe_is_asked_through_the_propagation_helper() {
        use anodizer_core::test_helpers::test_sources::{
            function_bodies, production_half, rust_sources,
        };

        let src = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src"));
        let landing = std::fs::read_to_string(src.join("landing.rs")).expect("read landing.rs");

        // The probe fields, read off the struct itself so a new probe joins
        // the population without anyone remembering to list it here.
        let fields: Vec<String> = landing
            .split("pub struct LandingProbes<'a> {")
            .nth(1)
            .expect("LandingProbes is declared here")
            .split("\n}")
            .next()
            .expect("the struct body ends")
            .lines()
            .filter_map(|l| l.trim().strip_prefix("pub "))
            .filter_map(|l| l.split(':').next())
            .filter(|f| *f != "propagation")
            .map(str::to_string)
            .collect();
        assert_eq!(
            fields.len(),
            6,
            "the probed publishers are cargo, npm, pypi, blob, snapcraft and docker: {fields:?}"
        );

        let mut askers = Vec::new();
        for source in rust_sources(src) {
            let text = std::fs::read_to_string(&source).expect("read source");
            for body in function_bodies(production_half(&text)) {
                let asked: Vec<&String> = fields
                    .iter()
                    .filter(|f| body.contains(&format!("probes.{f}")))
                    .collect();
                if asked.is_empty() {
                    continue;
                }
                let name = body.trim_start().lines().next().unwrap_or_default();
                assert!(
                    body.contains("probe_with_propagation"),
                    "{}: {name} asks {asked:?} outside the propagation helper",
                    source.display()
                );
                askers.extend(asked.into_iter().cloned());
            }
        }
        askers.sort();
        askers.dedup();
        let mut declared = fields.clone();
        declared.sort();
        assert_eq!(
            askers, declared,
            "every declared probe must have a caller that asks it"
        );
    }
}
