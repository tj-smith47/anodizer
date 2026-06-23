//! Shared test doubles for publisher dispatch tests.
//!
//! Gated as `#[cfg(any(test, feature = "test-support"))] #[doc(hidden)]
//! pub mod testing;` in `lib.rs`. The `test-support` Cargo feature is
//! enabled by this crate's own `[dev-dependencies]` so integration tests
//! under `tests/` can import the same fakes the in-crate unit tests
//! use. NOT a stable public API.

use anodizer_core::context::Context;
use anodizer_core::{PublishEvidence, Publisher, PublisherGroup, PublisherOutcome};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Drives [`FakePublisher::run`].
pub enum FakeOutcome {
    Succeed,
    Fail(String),
}

/// Drives [`FakePublisher::rollback`]. Independent from [`FakeOutcome`]
/// because publishing and rollback are exercised by separate tests:
/// rollback dispatch only walks publishers whose `run()` succeeded, so
/// the rollback-failure path needs `Succeed` for the publish side AND a
/// failing rollback to verify the per-step `RollbackFailed` outcome.
pub enum FakeRollback {
    Succeed,
    Fail(String),
}

/// Minimal [`Publisher`] implementation that records its identity and
/// returns a predetermined [`FakeOutcome`] from `run`.
pub struct FakePublisher {
    pub name: String,
    pub group: PublisherGroup,
    pub required: bool,
    pub outcome: FakeOutcome,
    pub rollback_outcome: FakeRollback,
    /// Mirrors [`Publisher::rollback_scope_needed`]. When `Some`, the
    /// rollback dispatcher checks for the corresponding env var before
    /// invoking `rollback()`.
    pub rollback_scope: Option<&'static str>,
    /// Mirrors [`Publisher::skips_on_nightly`]. When `true`, the dispatch
    /// loop records `Skipped(Nightly)` without invoking `run`.
    pub skips_on_nightly: bool,
}

impl Publisher for FakePublisher {
    fn name(&self) -> &str {
        &self.name
    }
    fn group(&self) -> PublisherGroup {
        self.group
    }
    fn required(&self) -> bool {
        self.required
    }
    fn skips_on_nightly(&self) -> bool {
        self.skips_on_nightly
    }
    fn run(&self, _ctx: &mut Context) -> anyhow::Result<PublishEvidence> {
        match &self.outcome {
            FakeOutcome::Succeed => Ok(PublishEvidence::new(self.name.clone())),
            FakeOutcome::Fail(msg) => anyhow::bail!("{}", msg),
        }
    }
    fn rollback(&self, _ctx: &mut Context, _evidence: &PublishEvidence) -> anyhow::Result<()> {
        match &self.rollback_outcome {
            FakeRollback::Succeed => Ok(()),
            FakeRollback::Fail(msg) => anyhow::bail!("{}", msg),
        }
    }
    fn rollback_scope_needed(&self) -> Option<&'static str> {
        self.rollback_scope
    }
}

/// Convenience constructor returning the boxed-trait-object shape the
/// dispatcher consumes. Defaults rollback to a no-op success and
/// declares no scope, matching the dispatcher's "rollback runs cleanly"
/// case used by every dispatch-side test.
pub fn fake(
    name: &str,
    group: PublisherGroup,
    required: bool,
    outcome: FakeOutcome,
) -> Box<dyn Publisher> {
    Box::new(FakePublisher {
        name: name.to_string(),
        group,
        required,
        outcome,
        rollback_outcome: FakeRollback::Succeed,
        rollback_scope: None,
        skips_on_nightly: false,
    })
}

/// Like [`fake`] but lets the test drive both the publish outcome AND
/// the rollback outcome. Use for rollback-failure tests where the
/// publisher must `Succeed` (so rollback dispatch picks it up) but the
/// `rollback()` call itself returns `Err`.
pub fn fake_with_rollback(
    name: &str,
    group: PublisherGroup,
    required: bool,
    outcome: FakeOutcome,
    rollback_outcome: FakeRollback,
) -> Box<dyn Publisher> {
    Box::new(FakePublisher {
        name: name.to_string(),
        group,
        required,
        outcome,
        rollback_outcome,
        rollback_scope: None,
        skips_on_nightly: false,
    })
}

/// Minimal [`Publisher`] that counts its `run()` and `rollback()`
/// invocations. Used by idempotency tests that need to assert
/// "rollback() was NOT called a second time" across two
/// `run_with_publishers` invocations, and by selection tests that need
/// to assert "run() was NEVER called" for a deselected publisher — the
/// standard [`FakePublisher`] exposes no such counters.
pub struct FakeCountingPublisher {
    pub name: String,
    pub group: PublisherGroup,
    pub required: bool,
    pub run_calls: Arc<AtomicUsize>,
    pub rollback_calls: Arc<AtomicUsize>,
}

impl Publisher for FakeCountingPublisher {
    fn name(&self) -> &str {
        &self.name
    }
    fn group(&self) -> PublisherGroup {
        self.group
    }
    fn required(&self) -> bool {
        self.required
    }
    fn skips_on_nightly(&self) -> bool {
        false
    }
    fn run(&self, _ctx: &mut Context) -> anyhow::Result<PublishEvidence> {
        self.run_calls.fetch_add(1, Ordering::SeqCst);
        Ok(PublishEvidence::new(self.name.clone()))
    }
    fn rollback(&self, _ctx: &mut Context, _evidence: &PublishEvidence) -> anyhow::Result<()> {
        self.rollback_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// Convenience constructor for [`FakeCountingPublisher`]. Returns the
/// boxed publisher alongside the shared *rollback* counter so the test
/// can assert `counter.load(Ordering::SeqCst) == N` after dispatch.
/// For the run-invocation counter, use [`fake_counting_runs`].
pub fn fake_counting(
    name: &str,
    group: PublisherGroup,
    required: bool,
) -> (Box<dyn Publisher>, Arc<AtomicUsize>) {
    let rollback_calls = Arc::new(AtomicUsize::new(0));
    let publisher = Box::new(FakeCountingPublisher {
        name: name.to_string(),
        group,
        required,
        run_calls: Arc::new(AtomicUsize::new(0)),
        rollback_calls: rollback_calls.clone(),
    });
    (publisher, rollback_calls)
}

/// Convenience constructor for [`FakeCountingPublisher`]. Returns the
/// boxed publisher alongside the shared *run* counter so the test can
/// assert that `run()` was (or was not) invoked — independent of the
/// recorded [`PublisherOutcome`]. For the rollback counter, use
/// [`fake_counting`].
pub fn fake_counting_runs(
    name: &str,
    group: PublisherGroup,
    required: bool,
) -> (Box<dyn Publisher>, Arc<AtomicUsize>) {
    let run_calls = Arc::new(AtomicUsize::new(0));
    let publisher = Box::new(FakeCountingPublisher {
        name: name.to_string(),
        group,
        required,
        run_calls: run_calls.clone(),
        rollback_calls: Arc::new(AtomicUsize::new(0)),
    });
    (publisher, run_calls)
}

/// Minimal [`Publisher`] whose `run()` returns `Ok` but records an
/// override [`PublisherOutcome`] on the context via
/// [`Context::record_publisher_outcome`]. Used by dispatch-level tests
/// that verify the override is respected (instead of the default
/// `Succeeded` mapping) — mirrors how chocolatey's moderation skip
/// and winget/krew/homebrew-cask's PR-already-exists skip report
/// `PendingModeration` / `PendingValidation` from a successful `run`.
pub struct FakeOutcomePublisher {
    pub name: String,
    pub group: PublisherGroup,
    pub required: bool,
    pub pending_outcome: PublisherOutcome,
}

impl Publisher for FakeOutcomePublisher {
    fn name(&self) -> &str {
        &self.name
    }
    fn group(&self) -> PublisherGroup {
        self.group
    }
    fn required(&self) -> bool {
        self.required
    }
    fn skips_on_nightly(&self) -> bool {
        false
    }
    fn run(&self, ctx: &mut Context) -> anyhow::Result<PublishEvidence> {
        ctx.record_publisher_outcome(self.pending_outcome.clone());
        Ok(PublishEvidence::new(self.name.clone()))
    }
    fn rollback(&self, _ctx: &mut Context, _evidence: &PublishEvidence) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Convenience constructor for [`FakeOutcomePublisher`].
pub fn fake_with_pending_outcome(
    name: &str,
    group: PublisherGroup,
    required: bool,
    pending_outcome: PublisherOutcome,
) -> Box<dyn Publisher> {
    Box::new(FakeOutcomePublisher {
        name: name.to_string(),
        group,
        required,
        pending_outcome,
    })
}

/// Like [`fake`] but declares a non-`None` `rollback_scope_needed`. Use
/// for the `RollbackSkippedNoScope` path where the dispatcher should
/// skip the rollback because the env var is unset.
pub fn fake_with_scope(
    name: &str,
    group: PublisherGroup,
    required: bool,
    outcome: FakeOutcome,
    rollback_scope: &'static str,
) -> Box<dyn Publisher> {
    Box::new(FakePublisher {
        name: name.to_string(),
        group,
        required,
        outcome,
        rollback_outcome: FakeRollback::Succeed,
        rollback_scope: Some(rollback_scope),
        skips_on_nightly: false,
    })
}

/// Like [`fake`] but sets `skips_on_nightly` to `true`. Use for tests that
/// exercise the nightly skip-list gate in the dispatch loop.
pub fn fake_with_nightly_skip(
    name: &str,
    group: PublisherGroup,
    required: bool,
    outcome: FakeOutcome,
) -> Box<dyn Publisher> {
    Box::new(FakePublisher {
        name: name.to_string(),
        group,
        required,
        outcome,
        rollback_outcome: FakeRollback::Succeed,
        rollback_scope: None,
        skips_on_nightly: true,
    })
}

/// Shared behavioral contract for per-crate publishers — catches the
/// silent-success class of bug where a publisher's `run()` returns `Ok`
/// with no operator-visible signal, indistinguishable from a real push
/// in the dispatch table.
///
/// The contract asserts the following hold after `publisher.run(&mut ctx)`
/// returns:
///
/// 1. `run()` returns `Ok(evidence)` and `evidence.publisher` matches
///    `publisher.name()`.
/// 2. At least three `status` log lines were emitted — the standard
///    start + per-crate-start + done pattern every per-crate publisher
///    uses. This is the load-bearing assertion that the publisher
///    actually visited its crates rather than silently `continue`-ing
///    through every iteration.
/// 3. Either:
///    - `evidence` is non-empty (`primary_ref` set or `extra` object
///      non-empty), **OR**
///    - the publisher emitted at least one `warn` line (e.g. the
///      no-eligible-crates remediation path fires), **OR**
///    - at least one extra `status` line beyond the standard three
///      was emitted — covering the dry-run / skip path where
///      publishers explicitly log `(dry-run) would push ...` instead
///      of recording rollback evidence. Without this branch the
///      contract would conflate "no evidence in dry-run" (correct;
///      avoids phantom rollback targets) with "silent success" (bug).
///
/// The capture is attached via [`Context::with_log_capture`] so every
/// logger built inside `publisher.run` records to the same vec without
/// any stderr-intercept gymnastics.
///
/// # Arguments
///
/// * `publisher` — the publisher under test.
/// * `ctx` — a context the caller built so the publisher's
///   configured-crate predicate selects at least one crate (typically
///   via [`anodizer_core::test_helpers::TestContextBuilder`] with a
///   single appropriately-configured crate and `dry_run(true)`).
///
/// # Panics
///
/// Panics (failing the test) if any contract invariant is violated.
pub fn assert_publisher_visible_work_contract(publisher: &dyn Publisher, ctx: &mut Context) {
    use anodizer_core::log::LogCapture;

    let capture = LogCapture::new();
    ctx.with_log_capture(capture.clone());

    let evidence = publisher
        .run(ctx)
        .unwrap_or_else(|e| panic!("publisher '{}' run() failed: {e:#}", publisher.name()));

    assert_eq!(
        evidence.publisher,
        publisher.name(),
        "evidence.publisher must match publisher.name()"
    );

    let status_count = capture.status_count();
    let verbose_count = capture.verbose_count();
    let warn_count = capture.warn_count();
    // Typed enum: `Empty` is the only "no operator-public fields"
    // variant. Anything else carries at least one target struct.
    let extra_is_empty = matches!(evidence.extra, anodizer_core::PublishEvidenceExtra::Empty);
    let has_evidence = evidence.primary_ref.is_some() || !extra_is_empty;

    // The publisher's default surface is a stage header + a done summary
    // (both at status); the per-crate-start line lives at verbose so the
    // per-crate × per-publisher loop stays quiet at default. Proof the
    // publisher actually visited a crate is the verbose per-crate-start
    // line, not a default status line.
    assert!(
        status_count >= 2,
        "publisher '{}': expected ≥2 status log lines (start + done), got {}. \
         Captured: {:?}",
        publisher.name(),
        status_count,
        capture.all_messages()
    );
    assert!(
        verbose_count >= 1,
        "publisher '{}': expected ≥1 verbose log line (per-crate-start), got {}. \
         This is the load-bearing proof the publisher visited a crate rather \
         than silently continuing. Captured: {:?}",
        publisher.name(),
        verbose_count,
        capture.all_messages()
    );

    // A publisher satisfies the visible-work contract when ANY of these
    // operator-readable signals is present:
    //   - evidence has content (production push completed), OR
    //   - a warn fired (no-eligible-crates explanation), OR
    //   - the loop emitted more than the bare start/done pair — i.e. at
    //     least one dry-run / skip / per-crate-progress status line, which
    //     is what tells the operator what would have happened on a real push.
    let extra_status_lines = status_count > 2;
    assert!(
        has_evidence || warn_count >= 1 || extra_status_lines,
        "publisher '{}': run() returned empty evidence AND emitted zero warnings AND \
         emitted no progress status beyond the standard start/done pair \
         — operator has no signal this publisher did anything. Captured: {:?}",
        publisher.name(),
        capture.all_messages()
    );
}

/// Create a throwaway git repo tagged `v0.1.0` and return its [`TempDir`]
/// handle (kept alive by the caller).
///
/// Every per-crate publisher's `run()` re-scopes each crate's version by
/// resolving its `tag_template` against `project_root` — which falls back to
/// the process cwd when a test leaves `project_root` unset. Pointing
/// `project_root` at this hermetic repo makes the version resolve from a
/// deterministic tag set rather than whatever git checkout the process cwd
/// happens to be inside, which a concurrently-running test in the same binary
/// can swap to a tag-less tempdir (via `CwdGuard`), starving the resolution and
/// flaking the run.
///
/// `#[cfg(test)]`-only: `init_git_repo_with_commits` lives behind
/// anodizer-core's `test-helpers` feature, which this crate enables only as a
/// dev-dependency, so the helper is unavailable to external `test-support`
/// consumers.
#[cfg(test)]
pub fn hermetic_tagged_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().expect("tempdir for hermetic publisher repo");
    // `init_git_repo_with_commits` writes a file per commit before committing,
    // so it works in an empty tempdir (plain `init_git_repo` assumes the caller
    // already wrote files and its `git commit` errors on an empty tree). The
    // first commit is tagged `v0.1.0`, matching the `v{{ .Version }}` template.
    anodizer_core::test_helpers::init_git_repo_with_commits(repo.path(), &["initial"]);
    repo
}

/// Create a throwaway git repo carrying an arbitrary set of tags and return its
/// [`TempDir`] handle (kept alive by the caller).
///
/// Unlike [`hermetic_tagged_repo`] (one `v0.1.0` tag), this seeds each tag in
/// `tags` onto the same initial commit so a workspace context with INDEPENDENT
/// per-crate `tag_template`s can resolve each crate's own version through the
/// production [`anodizer_core::crate_scope::resolve_crate_tag`] path. Pass tags
/// that match each crate's rendered `tag_template`, e.g.
/// `&["cfgd-core-v1.0.0", "cfgd-v2.0.0"]` for crates templated
/// `cfgd-core-v{{ .Version }}` / `cfgd-v{{ .Version }}` — the disjoint prefixes
/// keep each crate's regex from matching the sibling's tag.
///
/// `#[cfg(test)]`-only for the same reason as [`hermetic_tagged_repo`]:
/// `init_git_repo_with_commits` is behind anodizer-core's dev-only
/// `test-helpers` feature. The `git tag` spawns sit in a `#[cfg(test)]` helper,
/// covered by the module-boundaries test exemption.
#[cfg(test)]
pub fn hermetic_repo_with_tags(tags: &[&str]) -> tempfile::TempDir {
    let repo = tempfile::tempdir().expect("tempdir for hermetic per-crate repo");
    anodizer_core::test_helpers::init_git_repo_with_commits(repo.path(), &["initial"]);
    for tag in tags {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = std::process::Command::new("git");
                cmd.args(["tag", tag]).current_dir(repo.path());
                cmd
            },
            "git",
        );
        assert!(out.status.success(), "git tag {tag} exited non-zero");
    }
    repo
}
