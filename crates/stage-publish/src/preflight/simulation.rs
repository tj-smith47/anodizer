use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::preflight::{PreflightReport, PublisherState};
use anodizer_core::retry::RetryPolicy;
use anyhow::Result;

use super::*;

// ---------------------------------------------------------------------------
// crates.io publish-simulation preflight
// ---------------------------------------------------------------------------

/// Outcome of simulating one crate's `cargo publish --dry-run`.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum DryRunOutcome {
    /// The dry-run compiled and packaged cleanly.
    Ok,
    /// A verify/compile error — a dependent cannot build against the version
    /// of its dependency that the registry would resolve. Carries the matched
    /// stderr line so the abort message points the operator at the cause.
    CompileError(String),
    /// `cargo` resolved a sibling crate that is itself in the to-publish set
    /// but not yet on the registry. Benign during a real publish (cargo
    /// publishes siblings first), so it must NOT abort.
    BenignSiblingMissing(String),
    /// The dry-run could not run for an environmental reason (cargo absent,
    /// spawn failure). The caller degrades to the partial-publish check rather
    /// than failing the release on infrastructure.
    Unavailable(String),
}

/// Pluggable index-state query for the partial-publish check. Production wires
/// [`CargoCratesIo`]; tests inject a closure returning canned states without a
/// network round-trip.
type IndexQuery<'a> = dyn Fn(&str, &str) -> PublisherState + 'a;

/// Pluggable `cargo publish --dry-run` runner. Production wires
/// [`run_cargo_dry_run`] (a real spawn); tests inject a closure or drive the
/// real spawn against a PATH-injected `cargo` stub.
pub(super) type DryRunRunner<'a> = dyn Fn(&str) -> DryRunOutcome + 'a;

/// Simulate the crates.io publish BEFORE the irreversible cargo publisher
/// fires, aborting (via report blockers) when the workspace cannot publish
/// consistently at the target version.
///
/// Gated to a real release: snapshot / nightly / dry-run runs skip entirely
/// (they never reach crates.io, and the determinism harness rebuilds under
/// `--snapshot` must not spawn cargo or hit the network here).
///
/// Applies the real-release gate, then delegates to
/// [`run_cargo_publish_simulation_with`].
///
/// Both side-effecting seams are injected by the caller (the `run_preflight*`
/// entry points), so no test path ever hits the network or spawns cargo here:
/// - the partial-publish index query routes through the supplied
///   [`CheckerFactory`] — production uses [`RealCheckerFactory`] (a real
///   sparse-index GET); tests inject a mock factory returning canned states;
/// - the dry-run runner is the real `cargo publish --dry-run` spawn in
///   production and [`noop_dry_run_runner`] in every test seam.
pub(super) fn run_cargo_publish_simulation(
    ctx: &mut Context,
    log: &StageLogger,
    report: &mut PreflightReport,
    factory: &dyn CheckerFactory,
    dry_run_runner: &DryRunRunner<'_>,
) {
    // Gated to a REAL release that actually reaches crates.io. Snapshot /
    // nightly / dry-run never publish (and the determinism harness rebuilds
    // under `--snapshot --skip=...,publish`), and a skipped publish stage has
    // no irreversible door to guard — mirror the prepublish-guard gating.
    if ctx.is_snapshot() || ctx.is_nightly() || ctx.is_dry_run() || ctx.should_skip("publish") {
        return;
    }
    // Cargo out of the selected publish surface (e.g. `--publishers npm` or
    // `--skip cargo`) means the irreversible cargo door never fires this run,
    // so there is nothing to simulate — and spawning `cargo publish --dry-run`
    // or probing the crates.io index here would falsely abort a cargo-less
    // release on a cargo-only concern. Mirrors the state-probe gate above.
    if ctx.publisher_deselected("cargo") {
        log.verbose("cargo deselected from the publish surface; skipping cargo publish simulation");
        return;
    }

    let policy = RetryPolicy::PREFLIGHT;
    let checker = factory.cargo(policy);
    let index_query = |krate: &str, version: &str| checker.check(krate, version, log);

    run_cargo_publish_simulation_with(ctx, log, report, &index_query, dry_run_runner);
}

/// [`run_cargo_publish_simulation`] with the index query and dry-run runner
/// injected so tests can drive both checks without a network round-trip or a
/// real `cargo` (beyond a PATH-injected stub). Does NOT re-apply the
/// snapshot/nightly/dry-run or cargo-publisher-surface gates — the production
/// wrapper owns those so tests can exercise the logic directly.
pub(super) fn run_cargo_publish_simulation_with(
    ctx: &mut Context,
    log: &StageLogger,
    report: &mut PreflightReport,
    index_query: &IndexQuery<'_>,
    dry_run_runner: &DryRunRunner<'_>,
) {
    // Resolve the to-publish set via the same publish-graph derivation the
    // real cargo publisher uses, so the preflight can never disagree about
    // which crates publish or in what order.
    let selected = ctx.options.selected_crates.clone();
    let plan = match crate::cargo::cargo_publish_plan(ctx, &selected, log) {
        Ok(p) => p,
        Err(e) => {
            // A render failure in `skip:`/`if:` would also break the real
            // publish; surface it as a blocker rather than silently skipping.
            report
                .blockers
                .push(format!("cargo publish-simulation: {e:#}"));
            return;
        }
    };

    if plan.order.is_empty() {
        return;
    }

    let to_publish: Vec<(String, String)> = plan
        .order
        .iter()
        .map(|name| {
            let version = plan
                .versions
                .get(name)
                .cloned()
                .unwrap_or_else(|| ctx.version());
            (name.clone(), version)
        })
        .collect();

    // ---- (1) partial-publish probe (cheap, HTTP-only) --------------------
    if check_partial_publish(&to_publish, index_query, log, report) {
        // Only an Unknown transport error stops here — the registry state
        // couldn't be determined, so a resume can't be reasoned about safely.
        // A resumable mixed state warns and falls through to the completeness
        // guard (1b) and dry-run (2), which decide it precisely.
        return;
    }

    // ---- (1b) dep-completeness guard -------------------------------------
    // Refuse a release where a publishing crate depends on a workspace crate
    // that is neither in the publish set nor on crates.io. `cargo publish
    // --dry-run` (step 2) does NOT catch this — dry-run resolves the dep via
    // the local workspace PATH — so this guard is the only preflight check
    // for the missing-from-set failure that burned the 0.6.0/0.7.0 CLI
    // publish. Reuses the injected `index_query` so tests drive it without a
    // network round-trip; an inconclusive probe never blocks.
    if check_publish_set_completeness(&plan, index_query, log, report) {
        return;
    }

    // ---- (2) cargo publish --dry-run, dependency order -------------------
    simulate_dry_run_publishes(&to_publish, index_query, dry_run_runner, log, report);
}

/// Preflight wrapper over [`crate::cargo::check_publish_set_completeness`].
///
/// Maps the injected [`IndexQuery`] (which returns [`PublisherState`]) onto
/// the guard's tri-state probe, runs the guard, and converts a guard error
/// into a report blocker so the preflight aborts loudly (same channel as the
/// partial-publish and dry-run checks) instead of bubbling an `Err`. Returns
/// `true` when a blocker was pushed and the caller should stop.
fn check_publish_set_completeness(
    plan: &crate::cargo::CargoPublishPlan,
    index_query: &IndexQuery<'_>,
    log: &StageLogger,
    report: &mut PreflightReport,
) -> bool {
    use crate::cargo::DepIndexState;

    let probe = |name: &str, version: &str| match index_query(name, version) {
        PublisherState::Published => DepIndexState::Present,
        PublisherState::Clean => DepIndexState::Absent,
        // A transport error (Unknown) or any non-crates.io state is treated
        // conservatively — the guard does not fail on an inconclusive probe.
        _ => DepIndexState::Unknown,
    };

    match crate::cargo::check_publish_set_completeness(
        &plan.order,
        &plan.all_crates,
        &plan.versions,
        &probe,
        log,
    ) {
        Ok(()) => false,
        Err(e) => {
            report.blockers.push(format!("{e:#}"));
            true
        }
    }
}

/// (1) PARTIAL-PUBLISH PROBE.
///
/// Query each to-be-published crate's already-published state at its target
/// version and classify the set:
/// - MIXED (≥1 Published AND ≥1 Clean at the same V) → a RESUMABLE partial
///   publish: push a warning and return `false` (proceed). The Clean crates are
///   the ones this release publishes; the dep-completeness guard (1b) and the
///   dry-run (2) verify the resume actually completes, blocking only on a
///   genuine content conflict (a stale published dep surfaces as a dry-run
///   CompileError). Aborting here would strand every recoverable resume behind
///   a spurious version bump.
/// - any Unknown (transport error) → push a blocker (don't silently pass) and
///   return `true`. The registry state is undetermined, so a resume can't be
///   reasoned about safely.
/// - all-Clean → fresh release; return `false` (proceed to dry-run).
/// - all-Published → idempotent (the real publish would skip cargo); return
///   `false` (nothing to simulate, no abort).
///
/// Returns `true` only when a blocker was pushed (Unknown) and the caller stops.
fn check_partial_publish(
    to_publish: &[(String, String)],
    index_query: &IndexQuery<'_>,
    log: &StageLogger,
    report: &mut PreflightReport,
) -> bool {
    let mut first_published: Option<(String, String)> = None;
    let mut first_clean: Option<(String, String)> = None;

    for (name, version) in to_publish {
        log.verbose(&format!("checking crates.io state for '{name}@{version}'"));
        match index_query(name, version) {
            PublisherState::Published => {
                if first_published.is_none() {
                    first_published = Some((name.clone(), version.clone()));
                }
            }
            PublisherState::Clean => {
                if first_clean.is_none() {
                    first_clean = Some((name.clone(), version.clone()));
                }
            }
            PublisherState::Unknown { reason } => {
                report.blockers.push(format!(
                    "cargo publish-simulation: could not determine crates.io state for \
                     '{name}@{version}' ({reason}); refusing to start a release that may \
                     partially publish — retry once the registry is reachable"
                ));
                return true;
            }
            // crates.io never yields InModeration / PRPending; treat any other
            // state conservatively as "present" so a mixed set still aborts.
            other => {
                log.verbose(&format!(
                    "'{name}@{version}' reported {other}; treating as published"
                ));
                if first_published.is_none() {
                    first_published = Some((name.clone(), version.clone()));
                }
            }
        }
    }

    match (first_published, first_clean) {
        (Some((pub_name, pub_ver)), Some((clean_name, clean_ver))) => {
            // A mixed set is a RESUMABLE partial publish, not a dead end. The
            // Clean crates are exactly the ones this release publishes; the
            // Published ones skip idempotently. Whether the resume truly
            // completes depends on the published crates' content matching what
            // the Clean dependents build against — and that is precisely what
            // the dep-completeness guard (1b) and the `cargo publish --dry-run`
            // simulation (2) verify. Aborting here would strand every recoverable
            // partial publish (e.g. a re-run that only adds a crate the first
            // attempt missed from the publish set) behind a spurious version
            // bump. Warn and defer to the precise checks; only a genuine content
            // conflict — a Clean dependent that cannot build against a stale
            // published dep — blocks, via the dry-run's CompileError.
            report.warnings.push(format!(
                "crates.io version {pub_ver} is partially published ({pub_name}@{pub_ver} \
                 exists, {clean_name}@{clean_ver} does not) — resuming: the published crates \
                 skip idempotently and the remainder complete the version. Verifying \
                 completability via publish-simulation"
            ));
            false
        }
        // all-Published → idempotent skip; all-Clean → fresh; either proceeds.
        _ => false,
    }
}

/// (2) `cargo publish --dry-run` simulation in dependency order.
///
/// For each still-Clean crate (skipping any already-Published — the real
/// publish would skip those), run the dry-run and classify:
/// - [`DryRunOutcome::Ok`] → continue.
/// - [`DryRunOutcome::CompileError`] → push a blocker and stop: a dependent
///   cannot build against the dependency version the registry resolves (the
///   `probe_dir` failure mode).
/// - [`DryRunOutcome::BenignSiblingMissing`] → continue: cargo couldn't find a
///   sibling that is itself in the to-publish set and will be published first.
/// - [`DryRunOutcome::Unavailable`] → warn and fall back to check (1) (already
///   run) rather than hard-failing the release on infrastructure.
fn simulate_dry_run_publishes(
    to_publish: &[(String, String)],
    index_query: &IndexQuery<'_>,
    dry_run_runner: &DryRunRunner<'_>,
    log: &StageLogger,
    report: &mut PreflightReport,
) {
    let in_set: std::collections::HashSet<&str> =
        to_publish.iter().map(|(n, _)| n.as_str()).collect();

    for (name, version) in to_publish {
        // Skip crates already on the registry — the real publish skips them,
        // and `cargo publish --dry-run` would refuse an existing version.
        if matches!(index_query(name, version), PublisherState::Published) {
            continue;
        }

        log.verbose(&format!(
            "running cargo publish --dry-run -p {name} (publish simulation)"
        ));
        match dry_run_runner(name) {
            DryRunOutcome::Ok => {}
            DryRunOutcome::BenignSiblingMissing(detail) => {
                // Benign ONLY when the unresolved crate is itself in the
                // to-publish set (a sibling the real publish lands first). A
                // missing crate that is NOT in the set is a genuine resolution
                // failure that would also break the real publish — abort.
                if in_set.iter().any(|sib| detail.contains(sib)) {
                    log.verbose(&format!(
                        "'{name}' dry-run resolved a not-yet-published sibling \
                         ({detail}); benign — the real publish orders siblings first"
                    ));
                } else {
                    report.blockers.push(format!(
                        "cargo publish-simulation: `cargo publish --dry-run -p {name}` could \
                         not resolve a dependency ({detail}); it is not a workspace crate this \
                         release publishes, so the real publish would fail the same way — fix \
                         the dependency before releasing"
                    ));
                    return;
                }
            }
            DryRunOutcome::CompileError(detail) => {
                report.blockers.push(format!(
                    "cargo publish-simulation: `cargo publish --dry-run -p {name}` failed to \
                     build ({detail}); a published dependency is missing API this crate needs, \
                     so the real publish would fire the irreversible cargo publisher and then \
                     fail mid-release — bump to a new version or fix the dependency"
                ));
                return;
            }
            DryRunOutcome::Unavailable(detail) => {
                log.warn(&format!(
                    "skipped `cargo publish --dry-run -p {name}` — {detail} \
                     (relying on the partial-publish index check alone)"
                ));
            }
        }
    }
}

/// Spawn `cargo publish --dry-run -p <crate>` and classify the result.
///
/// Best-effort: a spawn failure (cargo absent / not executable) yields
/// [`DryRunOutcome::Unavailable`] so the caller degrades gracefully rather
/// than failing the release on a missing toolchain.
pub(super) fn run_cargo_dry_run(crate_name: &str, log: &StageLogger) -> DryRunOutcome {
    run_cargo_dry_run_with_binary(std::path::Path::new("cargo"), crate_name, log)
}

/// Path-taking sibling of [`run_cargo_dry_run`]: `cargo_binary` is the
/// binary to spawn. Production passes `Path::new("cargo")` (PATH
/// lookup); tests point at a nonexistent path to exercise the
/// spawn-failure branch without clobbering the process-wide `PATH`
/// (which would make every concurrent PATH-resolved spawn in the test
/// binary flaky). Same seam convention as
/// `core::git::gh_api_get_with_binary`.
pub(super) fn run_cargo_dry_run_with_binary(
    cargo_binary: &std::path::Path,
    crate_name: &str,
    log: &StageLogger,
) -> DryRunOutcome {
    run_cargo_dry_run_spawning(cargo_binary, crate_name, log, |cmd| cmd.output())
}

/// Spawn-seam sibling of [`run_cargo_dry_run_with_binary`]: `spawn` builds the
/// `cargo publish --dry-run` output. Production passes `|cmd| cmd.output()`;
/// tests that install a `FakeToolDir` stub and exec it immediately inject
/// `output_retrying_etxtbsy` so the write-then-exec `ETXTBSY` race a sibling
/// test thread's `fork` window opens does not flake the real spawn+classify
/// path. Production never writes-then-execs `cargo`, so its raw `.output()`
/// cannot hit that race.
pub(super) fn run_cargo_dry_run_spawning(
    cargo_binary: &std::path::Path,
    crate_name: &str,
    log: &StageLogger,
    spawn: impl FnOnce(&mut std::process::Command) -> std::io::Result<std::process::Output>,
) -> DryRunOutcome {
    use std::process::Command;

    let mut cmd = Command::new(cargo_binary);
    cmd.args(["publish", "--dry-run", "-p", crate_name]);

    let output = match spawn(&mut cmd) {
        Ok(o) => o,
        Err(e) => return DryRunOutcome::Unavailable(format!("spawn cargo: {e}")),
    };

    if output.status.success() {
        return DryRunOutcome::Ok;
    }

    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    log.verbose(&format!(
        "`cargo publish --dry-run -p {crate_name}` exited non-zero:\n{}",
        anodizer_core::redact::redact_bearer_tokens(stderr.trim_end())
    ));
    classify_dry_run_stderr(&stderr)
}

/// Classify a failed `cargo publish --dry-run` stderr into a [`DryRunOutcome`].
///
/// - A "did not match any packages" / "package ID specification" line means
///   `-p <crate>` didn't resolve to a workspace member in THIS context (a
///   degenerate/test invocation, never a real release) → [`DryRunOutcome::Unavailable`].
/// - A "no matching package" / "failed to select a version" line naming a
///   crate is treated as a (potentially benign) missing-sibling signal: the
///   caller decides benign-vs-blocker by checking whether the named crate is
///   in the to-publish set.
/// - A compile/verify error (`error[E…]`, "cannot find …", "could not
///   compile") is a hard [`DryRunOutcome::CompileError`].
/// - Anything else non-zero is conservatively a `CompileError` (an unexpected
///   verify failure should abort, not pass) — except a pure registry/network
///   complaint, which degrades to `Unavailable`.
pub(super) fn classify_dry_run_stderr(stderr: &str) -> DryRunOutcome {
    let lower = stderr.to_ascii_lowercase();

    // Invocation/environment artifact: the crate named by `-p` is not a
    // resolvable workspace member in THIS context (e.g. a synthetic test
    // fixture, or a sparse checkout). In a real release the crate IS a
    // workspace member, so this string only appears in degenerate contexts —
    // treat it as Unavailable (warn + fall back to the index check), never a
    // hard blocker.
    if lower.contains("did not match any packages") || lower.contains("package id specification") {
        return DryRunOutcome::Unavailable(first_nonempty_line(stderr));
    }

    // Missing-package signal (a dependency cargo could not resolve on the
    // registry). Carry the offending line; the caller distinguishes a
    // to-publish sibling (benign) from a genuinely-absent external dep.
    if lower.contains("no matching package")
        || lower.contains("failed to select a version")
        || lower.contains("could not find")
    {
        let line = first_line_matching(
            stderr,
            &["no matching package", "failed to select", "could not find"],
        );
        return DryRunOutcome::BenignSiblingMissing(line);
    }

    // Compile / verify failure — the dependent can't build against its
    // published dependency (the probe_dir case).
    if lower.contains("error[e")
        || lower.contains("cannot find function")
        || lower.contains("cannot find ")
        || lower.contains("could not compile")
        || lower.contains("unresolved import")
    {
        let line = first_line_matching(
            stderr,
            &[
                "error[e",
                "cannot find",
                "could not compile",
                "unresolved import",
            ],
        );
        return DryRunOutcome::CompileError(line);
    }

    // Pure registry/network failure → environmental, not a code problem.
    if lower.contains("failed to download")
        || lower.contains("network failure")
        || lower.contains("spurious network error")
        || lower.contains("error: failed to get successful http response")
    {
        return DryRunOutcome::Unavailable(first_nonempty_line(stderr));
    }

    // Unknown non-zero exit: conservatively treat as a compile/verify failure
    // so a real problem aborts rather than slipping past the gate.
    DryRunOutcome::CompileError(first_nonempty_line(stderr))
}

/// First stderr line containing any of `needles` (case-insensitive), trimmed.
/// Falls back to the first non-empty line.
pub(super) fn first_line_matching(stderr: &str, needles: &[&str]) -> String {
    for line in stderr.lines() {
        let lower = line.to_ascii_lowercase();
        if needles.iter().any(|n| lower.contains(n)) {
            return line.trim().to_string();
        }
    }
    first_nonempty_line(stderr)
}

/// First non-empty, trimmed stderr line (or a placeholder when stderr is bare).
pub(super) fn first_nonempty_line(stderr: &str) -> String {
    stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("non-zero exit, no diagnostic")
        .to_string()
}

// ---------------------------------------------------------------------------
// Rollback-scope + publisher-preflight extension
// ---------------------------------------------------------------------------

/// Walk the trait-based publisher registry and surface two classes of
/// resilience concerns into the report:
///
/// 1. Rollback scope availability — every publisher whose
///    [`Publisher::rollback_scope_needed`] returns `Some(label)` is checked
///    against the env var named in `label`. Missing scope becomes a
///    warning by default and a blocker under `--strict`. If
///    `--rollback=best-effort` was explicitly requested and any
///    `required` publisher lacks rollback scope, this function returns
///    `Err` so the CLI bails before any publish work runs.
/// 2. Publisher self-check — each publisher's [`Publisher::preflight`]
///    return value is folded into the report (`Warning` -> warnings,
///    `Blocker` -> blockers, `Err` -> blockers tagged as preflight error).
///    All publishers currently return `Pass`; the wiring is here so
///    future per-publisher preflight logic flows through the same channel.
pub(super) fn run_publisher_preflight_extension(
    ctx: &anodizer_core::context::Context,
    report: &mut PreflightReport,
    // Publisher `preflight()` hooks can perform live credential / repo probes
    // (cargo/npm token validity, GitHub-repo write scope, AUR ssh auth). The
    // production `run_preflight` enables them; the injected-factory test seams
    // disable them so unit tests stay hermetic (the rollback-scope branch below
    // is pure and always runs).
    live_publisher_preflight: bool,
) -> Result<()> {
    let publishers = crate::registry::configured_publishers(ctx);
    let mut required_missing_scope: Vec<String> = Vec::new();

    for p in &publishers {
        // Mirror the run path's skip set (`dispatch::dispatch`): a publisher
        // the release will not run must contribute nothing to the gate — no
        // live credential/repo probe AND no rollback-scope blocker. Probing a
        // deselected (`--skip`/`--publishers`) or nightly-skipped publisher can
        // manufacture a false Blocker against an irreversible door that never
        // opens this run.
        if ctx.publisher_deselected(p.name()) || (ctx.is_nightly() && p.skips_on_nightly()) {
            continue;
        }

        // ---- rollback scope check ------------------------------------
        if let Some(label) = p.rollback_scope_needed()
            && !crate::scope::scope_available_with_env(label, ctx.env_source())
        {
            let msg = crate::scope::warn_scope_unavailable_msg("preflight", p.name(), label);
            if ctx.options.strict {
                report.blockers.push(msg);
            } else {
                report.warnings.push(msg);
            }
            if p.required() {
                required_missing_scope.push(p.name().to_string());
            }
        }

        // ---- publisher self-check ------------------------------------
        if !live_publisher_preflight {
            continue;
        }
        match p.preflight(ctx) {
            Ok(anodizer_core::PreflightCheck::Pass) => {}
            Ok(anodizer_core::PreflightCheck::Warning(msg)) => {
                report.warnings.push(format!("{}: {}", p.name(), msg));
            }
            Ok(anodizer_core::PreflightCheck::Blocker(msg)) => {
                report.blockers.push(format!("{}: {}", p.name(), msg));
            }
            Err(err) => {
                report
                    .blockers
                    .push(format!("{}: preflight error: {}", p.name(), err));
            }
        }
    }

    // Hard error: `--rollback=best-effort` was explicitly requested but a
    // required publisher lacks rollback scope. Bail before any side-effect
    // stage runs so the operator can elevate the token (or accept losing
    // rollback) before starting a release that cannot recover from failure.
    if matches!(
        ctx.options.rollback_mode,
        Some(anodizer_core::context::RollbackMode::BestEffort)
    ) && !required_missing_scope.is_empty()
    {
        anyhow::bail!(
            "preflight: --rollback=best-effort was requested but the following required publishers lack rollback scope: {}",
            required_missing_scope.join(", "),
        );
    }

    Ok(())
}
