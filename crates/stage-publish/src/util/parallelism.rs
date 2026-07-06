//! Shared rollback fan-out primitives used by every publisher whose
//! rollback path issues one network/git call per recorded target.
//!
//! Artifactory's rollback first established the cap; promoting it here
//! lets the git-revert publishers (homebrew / scoop / nix / aur) and
//! any future publisher reuse the same operator-friendly limit and the
//! same [`std::thread::scope`] fan-out shape without copy-pasting the
//! loop.
//!
//! See [`crate::util::git_revert`] for the per-target work the
//! git-revert publishers drive through this primitive.

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use std::sync::Mutex;
use std::thread::ScopedJoinHandle;

use super::git_revert::{RevertTarget, run_git_revert_and_push};

// `lock_recover` is the canonical poisoned-mutex recovery helper in
// `anodizer_core::parallel`; re-exported here so existing `crate::util::lock_recover`
// call sites in sibling publisher modules keep compiling without a path change.
pub(crate) use anodizer_core::parallel::lock_recover;

/// Join a scoped worker thread, logging a warn line on panic instead
/// of silently dropping the join error.
///
/// `label` names the publisher / phase so operators can correlate the
/// log with the surrounding "closed N, failed M" summary. The panic
/// payload's most common shapes (`&'static str` / `String`) are
/// downcast to surface a readable message; other payload types fall
/// back to `{:?}` rather than vanishing.
///
/// Accepts [`ScopedJoinHandle`] (not [`std::thread::JoinHandle`])
/// because every caller drives workers through [`std::thread::scope`]
/// — that's what makes it safe to borrow `&Mutex` / `&StageLogger`
/// across thread boundaries without a `'static` bound.
pub(crate) fn join_or_warn<'scope, T>(
    h: ScopedJoinHandle<'scope, T>,
    log: &StageLogger,
    label: &str,
) {
    if let Err(panic_payload) = h.join() {
        // Try the two common payload types first so the warn line
        // shows a readable string rather than the `Any` placeholder.
        let msg = if let Some(s) = panic_payload.downcast_ref::<&'static str>() {
            (*s).to_string()
        } else if let Some(s) = panic_payload.downcast_ref::<String>() {
            s.clone()
        } else {
            format!("{:?}", panic_payload)
        };
        log.warn(&format!("{label} worker thread panicked: {msg}"));
    }
}

/// Maximum concurrent rollback workers per publisher.
///
/// Chosen to match the scale at which v0.2.0's 143-artifact
/// artifactory cascade case becomes operator-usable (~36 batches of
/// 4 at 30s/req) without exhausting any reasonable remote rate
/// limit. The git-revert + push pattern is bounded by the user's
/// network and the git remote's per-IP push rate, both of which 4
/// stays comfortably under.
pub(crate) const ROLLBACK_PARALLELISM: usize = 4;

/// Fan out [`run_git_revert_and_push`] across `targets` under the
/// [`ROLLBACK_PARALLELISM`] cap and return `(reverted, failed)`
/// counts.
///
/// `publisher` and `env_var_hint` are forwarded to the canonical
/// [`crate::publisher_helpers::rollback_failure_warning_msg`] so the
/// per-failure warn line names the env var (or AUR SSH key) the
/// operator must restore.
///
/// Each chunk uses [`std::thread::scope`] (no `'static` bounds
/// needed; the `&[RevertTarget]` and `&StageLogger` slice references
/// remain valid for the lifetime of the scope). Failures are
/// captured by counter increment + warn, NOT short-circuited — one
/// auth failure on target 1 must not skip targets 2..n.
pub(crate) fn run_revert_targets_parallel(
    targets: &[RevertTarget],
    publisher: &str,
    env_var_hint: Option<&str>,
    log: &StageLogger,
) -> (usize, usize) {
    let counts = Mutex::new((0usize, 0usize));
    let chunks = targets.chunks(ROLLBACK_PARALLELISM);
    for chunk in chunks {
        std::thread::scope(|s| {
            let mut handles = Vec::with_capacity(chunk.len());
            for target in chunk {
                let log = log.clone();
                let counts = &counts;
                handles.push(s.spawn(move || {
                    log.status(&format!(
                        "reverting and pushing {} for {} ({})",
                        target.target, publisher, target.repo_url
                    ));
                    match run_git_revert_and_push(target, &log) {
                        Ok(()) => {
                            let mut c = lock_recover(counts, &log, publisher);
                            c.0 += 1;
                        }
                        Err(err) => {
                            let mut c = lock_recover(counts, &log, publisher);
                            c.1 += 1;
                            log.warn(&crate::publisher_helpers::rollback_failure_warning_msg(
                                publisher,
                                &target.target,
                                &target.repo_url,
                                &err,
                                env_var_hint,
                            ));
                        }
                    }
                }));
            }
            for h in handles {
                join_or_warn(h, log, publisher);
            }
        });
    }
    // `into_inner` consumes the Mutex — poison here would mean a worker
    // panicked while holding the guard. Counters are still readable, so
    // recover rather than abandon the operator-facing summary.
    match counts.into_inner() {
        Ok(c) => c,
        Err(poisoned) => {
            log.warn(&format!(
                "{publisher} mutex poisoned by worker panic; reporting counters as-of poison"
            ));
            poisoned.into_inner()
        }
    }
}

/// Read-only view of a token-authenticated git-revert rollback target.
///
/// Every "token publisher" (scoop / homebrew / nix) records the same field
/// set in its evidence snapshot — `target` / `repo_url` / `branch` /
/// `token_env_var` — and reverts via an HTTPS clone authenticated by a
/// `token_env_var`-named secret. This trait lets
/// [`run_token_revert_rollback`] map any such snapshot into the shared
/// [`RevertTarget`] without re-typing the conversion per publisher.
///
/// The AUR publisher is deliberately NOT a `TokenRevertTarget`: it
/// authenticates with SSH key material (`private_key` + `ssh_command`)
/// resolved from config at rollback time, not an HTTPS token env var, so it
/// keeps its own `rollback()` body while still sharing the
/// [`run_revert_targets_parallel`] fan-out core.
pub(crate) trait TokenRevertTarget {
    /// Per-target label (crate / formula / cask / manifest name) surfaced in
    /// rollback warn lines.
    fn target(&self) -> &str;
    /// HTTPS clone URL of the publisher-owned repo to revert.
    fn repo_url(&self) -> &str;
    /// Branch the publish pushed to; `None` means the cloned default branch.
    fn branch(&self) -> Option<&str>;
    /// NAME of the env var holding the rollback re-clone token (never the
    /// token VALUE — that is resolved from the live env at rollback time).
    fn token_env_var(&self) -> Option<&str>;
}

macro_rules! impl_token_revert_target {
    ($t:ty) => {
        impl TokenRevertTarget for $t {
            fn target(&self) -> &str {
                &self.target
            }
            fn repo_url(&self) -> &str {
                &self.repo_url
            }
            fn branch(&self) -> Option<&str> {
                self.branch.as_deref()
            }
            fn token_env_var(&self) -> Option<&str> {
                self.token_env_var.as_deref()
            }
        }
    };
}

impl_token_revert_target!(anodizer_core::publish_evidence::ScoopTargetSnapshot);
impl_token_revert_target!(anodizer_core::publish_evidence::HomebrewTargetSnapshot);
impl_token_revert_target!(anodizer_core::publish_evidence::NixTargetSnapshot);

/// Drive the full token-publisher rollback for a set of already-deduped
/// [`TokenRevertTarget`]s: resolve each target's token from the live env,
/// map to [`RevertTarget`], fan out [`run_revert_targets_parallel`], and
/// emit the per-publisher summary line.
///
/// Collapses the byte-identical `rollback()` bodies of scoop / homebrew /
/// nix into one call. The caller supplies the decode + dedup (its evidence
/// variant and `(repo_url, branch)` dedup are publisher-typed) plus the
/// publisher's nouns:
/// - `default_env_hint` — token env var named in failure warns when a
///   target carries none (e.g. `HOMEBREW_TAP_TOKEN`).
/// - `empty_evidence_noun` — what the empty-evidence warn calls the missing
///   targets (e.g. `tap clone targets`).
/// - `reverted_noun` — the unit pluralized in the summary line (e.g. `tap`
///   → `reverted N tap(s)`).
pub(crate) fn run_token_revert_rollback<T: TokenRevertTarget>(
    ctx: &Context,
    deduped_targets: &[T],
    publisher: &str,
    default_env_hint: &str,
    empty_evidence_noun: &str,
    reverted_noun: &str,
) -> anyhow::Result<()> {
    let log = ctx.logger("publish");
    if deduped_targets.is_empty() {
        log.warn(&crate::publisher_helpers::rollback_empty_warning_msg(
            publisher,
            empty_evidence_noun,
        ));
        return Ok(());
    }
    // Resolve auth tokens at rollback time — never persisted in evidence.
    // `token_env_var` is only the NAME of the env var; the value lives in
    // the injected env source.
    let env = ctx.env_source();
    let prepared: Vec<RevertTarget> = deduped_targets
        .iter()
        .map(|t| RevertTarget {
            target: t.target().to_string(),
            repo_url: t.repo_url().to_string(),
            branch: t.branch().map(str::to_string),
            token: crate::util::resolve_rollback_token(env, t.token_env_var()),
            private_key: None,
            ssh_command: None,
        })
        .collect();
    // Every target in one publisher's rollback carries the same env-var hint
    // by construction; the first target's is representative.
    let env_hint = deduped_targets
        .first()
        .and_then(|t| t.token_env_var())
        .unwrap_or(default_env_hint);
    let (reverted, failed) =
        run_revert_targets_parallel(&prepared, publisher, Some(env_hint), &log);
    log.status(&format!(
        "{publisher} rollback reverted {reverted} {reverted_noun}(s), {failed} failure(s)"
    ));
    // A per-target git-revert failure must surface as `Err` here so
    // `execute_rollback_step` maps this publisher's row to
    // `RollbackFailed`/`RollbackDisposition::Failed` instead of `RolledBack`
    // — otherwise the outer `rollback complete — N rolled back, M failed`
    // summary miscounts an unreverted index repo as a success.
    if failed > 0 {
        anyhow::bail!(
            "{publisher} rollback: {failed} of {} {reverted_noun}(s) failed to revert (see per-target warnings above)",
            prepared.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::log::{StageLogger, Verbosity};
    use std::process::Command;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Ensure the test process has a git identity. Subprocess `git`
    /// invocations inside the helper inherit env from the parent
    /// process; without these env vars they bail with "Author identity
    /// unknown" on minimal CI runners.
    fn ensure_git_identity() {
        static INIT: OnceLock<()> = OnceLock::new();
        // SAFETY: env mutation runs exactly once per process, guarded by
        // OnceLock; no other test thread observes a half-applied identity.
        // The values are constants, idempotently set and never removed.
        INIT.get_or_init(|| unsafe {
            // env-ok: idempotent OnceLock set of constant git identity, never mutated after
            std::env::set_var("GIT_AUTHOR_NAME", "Anodize Test");
            // env-ok: idempotent OnceLock set of constant git identity, never mutated after
            std::env::set_var("GIT_AUTHOR_EMAIL", "test@anodize.local");
            // env-ok: idempotent OnceLock set of constant git identity, never mutated after
            std::env::set_var("GIT_COMMITTER_NAME", "Anodize Test");
            // env-ok: idempotent OnceLock set of constant git identity, never mutated after
            std::env::set_var("GIT_COMMITTER_EMAIL", "test@anodize.local");
        });
    }

    /// Build a bare remote with one commit on `master`, suitable as a
    /// target for `run_git_revert_and_push`. Returns the bare-repo path
    /// (usable as `repo_url`) plus the two TempDir holders to keep on
    /// the test's stack.
    fn bare_with_one_commit() -> (String, tempfile::TempDir, tempfile::TempDir) {
        ensure_git_identity();
        let bare = tempfile::tempdir().expect("bare");
        let work = tempfile::tempdir().expect("work");
        assert!(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["init", "--bare", "-b", "master"])
                        .arg(bare.path());
                    cmd
                },
                "git",
            )
            .status
            .success()
        );
        for args in [
            vec!["init", "-b", "master"],
            vec!["config", "user.email", "t@example.invalid"],
            vec!["config", "user.name", "T"],
            vec!["config", "commit.gpgsign", "false"],
        ] {
            assert!(
                anodizer_core::test_helpers::output_with_spawn_retry(
                    || {
                        let mut cmd = Command::new("git");
                        cmd.args(&args).current_dir(work.path());
                        cmd
                    },
                    "git",
                )
                .status
                .success()
            );
        }
        std::fs::write(work.path().join("README"), "hi\n").unwrap();
        for args in [vec!["add", "README"], vec!["commit", "-m", "initial"]] {
            assert!(
                anodizer_core::test_helpers::output_with_spawn_retry(
                    || {
                        let mut cmd = Command::new("git");
                        cmd.args(&args).current_dir(work.path());
                        cmd
                    },
                    "git",
                )
                .status
                .success()
            );
        }
        assert!(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["remote", "add", "origin"])
                        .arg(bare.path())
                        .current_dir(work.path());
                    cmd
                },
                "git",
            )
            .status
            .success()
        );
        assert!(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["push", "-u", "origin", "master"])
                        .current_dir(work.path());
                    cmd
                },
                "git",
            )
            .status
            .success()
        );
        (bare.path().to_string_lossy().into_owned(), bare, work)
    }

    /// Build a target pointed at the given URL. Local-path URLs (which
    /// don't start with `https://`) take the SSH dispatch branch inside
    /// `run_git_revert_and_push`, which is fine for local bare remotes.
    fn target(label: &str, url: &str) -> RevertTarget {
        RevertTarget {
            target: label.into(),
            repo_url: url.into(),
            branch: Some("master".into()),
            token: None,
            private_key: None,
            ssh_command: None,
        }
    }

    /// Empty input must return (0, 0) without spawning a scope.
    /// Guards against a panic in `chunks(0)`-like degenerate cases.
    #[test]
    fn run_revert_targets_parallel_handles_empty_slice() {
        let log = StageLogger::new("test", Verbosity::Normal);
        let (ok, err) = run_revert_targets_parallel(&[], "homebrew", Some("X"), &log);
        assert_eq!(ok, 0);
        assert_eq!(err, 0);
    }

    /// Happy path: every target points at a real bare remote with a
    /// revertable HEAD. The fan-out must report `(N, 0)` and the bare
    /// remotes must each show a fresh `Revert "..."` commit at HEAD.
    #[test]
    fn run_revert_targets_parallel_counts_all_successes() {
        let log = StageLogger::new("test", Verbosity::Quiet);
        // Hold every TempDir on the test stack until after the call —
        // dropping a `bare` TempDir mid-flight would yank the remote.
        let remotes: Vec<_> = (0..3).map(|_| bare_with_one_commit()).collect();
        let targets: Vec<RevertTarget> = remotes
            .iter()
            .enumerate()
            .map(|(i, (url, _, _))| target(&format!("t{i}"), url))
            .collect();

        let (ok, err) = run_revert_targets_parallel(&targets, "homebrew", Some("HB"), &log);
        assert_eq!(ok, 3, "all three targets should report success");
        assert_eq!(err, 0, "no failures expected on clean bare remotes");

        // Independently verify a revert commit landed on each bare. Fresh
        // shallow clone + log -1 — same shape as git_revert.rs's
        // verification pattern.
        for (url, _, _) in &remotes {
            let verify = tempfile::tempdir().unwrap();
            assert!(
                anodizer_core::test_helpers::output_with_spawn_retry(
                    || {
                        let mut cmd = Command::new("git");
                        cmd.args(["clone", "--depth=2", url])
                            .arg(verify.path().join("repo"));
                        cmd
                    },
                    "git",
                )
                .status
                .success()
            );
            let out = anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["log", "-1", "--pretty=%s"])
                        .current_dir(verify.path().join("repo"));
                    cmd
                },
                "git",
            );
            let subject = String::from_utf8_lossy(&out.stdout).trim().to_string();
            assert!(
                subject.starts_with("Revert"),
                "expected revert commit on {url}, got subject={subject:?}"
            );
        }
    }

    /// Failure isolation: a bad URL must NOT short-circuit the other
    /// targets. The bad target reports as a failure (counter increments,
    /// warn emitted naming the env-var hint), while sibling targets
    /// still succeed.
    #[test]
    fn run_revert_targets_parallel_isolates_failures() {
        let (log, cap) = StageLogger::with_capture("test", Verbosity::Normal);
        let (good_url, _good_bare, _good_work) = bare_with_one_commit();
        let targets = vec![
            target("good", &good_url),
            // Local path that does not exist — clone will fail with
            // a non-zero exit, surfacing as a per-target error.
            target("bad", "/this/path/must/not/exist/anywhere/zzz.git"),
        ];

        let (ok, err) = run_revert_targets_parallel(&targets, "scoop", Some("SCOOP_KEY"), &log);
        assert_eq!(ok, 1, "the good target must still complete");
        assert_eq!(err, 1, "the bad target must register as a failure");

        // The per-failure warn line is routed through
        // `publisher_helpers::rollback_failure_warning_msg`, which
        // embeds the publisher name and the env-var hint so the
        // operator knows which credential to restore.
        let warns: Vec<String> = cap
            .all_messages()
            .into_iter()
            .filter_map(|(lvl, m)| (lvl == anodizer_core::log::LogLevel::Warn).then_some(m))
            .collect();
        assert!(
            warns.iter().any(|m| m.contains("scoop")),
            "expected publisher name in warn, got: {warns:?}"
        );
        assert!(
            warns.iter().any(|m| m.contains("SCOOP_KEY")),
            "expected env-var hint in warn, got: {warns:?}"
        );
    }

    /// Chunking: more targets than `ROLLBACK_PARALLELISM` must all be
    /// processed, not just the first batch. This is the regression
    /// guard for the chunk-loop boundary (`chunks(cap)` must visit
    /// every chunk, not stop after one).
    #[test]
    fn run_revert_targets_parallel_processes_all_chunks() {
        let log = StageLogger::new("test", Verbosity::Quiet);
        // Pick a count strictly greater than the cap so we exercise
        // ≥2 chunks. Cap is 4 at time of writing; 6 ⇒ 2 chunks (4 + 2).
        let n = ROLLBACK_PARALLELISM + 2;
        assert!(n > ROLLBACK_PARALLELISM, "must span >1 chunk");
        let remotes: Vec<_> = (0..n).map(|_| bare_with_one_commit()).collect();
        let targets: Vec<RevertTarget> = remotes
            .iter()
            .enumerate()
            .map(|(i, (url, _, _))| target(&format!("c{i}"), url))
            .collect();

        let (ok, err) = run_revert_targets_parallel(&targets, "nix", None, &log);
        assert_eq!(ok, n, "every chunk's targets must be processed");
        assert_eq!(err, 0);
    }

    /// `join_or_warn` on a worker that returned normally must NOT emit
    /// a warn — the join-error path is reserved for thread panics.
    #[test]
    fn join_or_warn_silent_on_clean_return() {
        let (log, cap) = StageLogger::with_capture("test", Verbosity::Normal);
        let counter = AtomicUsize::new(0);
        std::thread::scope(|s| {
            let h = s.spawn(|| {
                counter.fetch_add(1, Ordering::SeqCst);
            });
            join_or_warn(h, &log, "publisher-X");
        });
        assert_eq!(counter.load(Ordering::SeqCst), 1, "worker must have run");
        assert_eq!(cap.warn_count(), 0, "clean join must not warn");
    }

    /// `join_or_warn` on a panicking worker must emit one warn line
    /// naming the publisher label AND surfacing the panic payload as a
    /// readable string (NOT the `Any { .. }` placeholder).
    #[test]
    fn join_or_warn_logs_string_panic_payload() {
        let (log, cap) = StageLogger::with_capture("test", Verbosity::Normal);
        std::thread::scope(|s| {
            let h = s.spawn(|| {
                panic!("boom-from-worker");
            });
            join_or_warn(h, &log, "publisher-Y");
        });
        let warns: Vec<String> = cap
            .all_messages()
            .into_iter()
            .filter_map(|(lvl, m)| (lvl == anodizer_core::log::LogLevel::Warn).then_some(m))
            .collect();
        assert_eq!(warns.len(), 1, "exactly one warn for one panicked worker");
        let msg = &warns[0];
        assert!(
            msg.contains("publisher-Y"),
            "warn must name the publisher label, got: {msg}"
        );
        assert!(
            msg.contains("boom-from-worker"),
            "warn must surface the panic payload, got: {msg}"
        );
    }

    /// `run_token_revert_rollback` delegates to the shared fan-out: every
    /// deduped [`TokenRevertTarget`] (here `ScoopTargetSnapshot`) is
    /// reverted exactly once and the per-publisher summary line is emitted
    /// with the supplied `reverted_noun`.
    #[test]
    fn run_token_revert_rollback_reverts_each_target_and_summarizes() {
        use anodizer_core::log::LogLevel;
        use anodizer_core::publish_evidence::ScoopTargetSnapshot;
        use anodizer_core::test_helpers::TestContextBuilder;

        let r0 = bare_with_one_commit();
        let r1 = bare_with_one_commit();
        let targets = vec![
            ScoopTargetSnapshot {
                target: "a".into(),
                repo_url: r0.0.clone(),
                branch: Some("master".into()),
                token_env_var: None,
            },
            ScoopTargetSnapshot {
                target: "b".into(),
                repo_url: r1.0.clone(),
                branch: Some("master".into()),
                token_env_var: None,
            },
        ];

        let mut ctx = TestContextBuilder::new().build();
        let cap = anodizer_core::log::LogCapture::new();
        ctx.with_log_capture(cap.clone());

        run_token_revert_rollback(
            &ctx,
            &targets,
            "scoop",
            "SCOOP_BUCKET_TOKEN",
            "bucket clone targets",
            "bucket",
        )
        .expect("rollback should succeed on clean bare remotes");

        for url in [&r0.0, &r1.0] {
            let verify = tempfile::tempdir().unwrap();
            assert!(
                anodizer_core::test_helpers::output_with_spawn_retry(
                    || {
                        let mut cmd = Command::new("git");
                        cmd.args(["clone", "--depth=2", url])
                            .arg(verify.path().join("repo"));
                        cmd
                    },
                    "git",
                )
                .status
                .success()
            );
            let out = anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["log", "-1", "--pretty=%s"])
                        .current_dir(verify.path().join("repo"));
                    cmd
                },
                "git",
            );
            let subject = String::from_utf8_lossy(&out.stdout).trim().to_string();
            assert!(
                subject.starts_with("Revert"),
                "expected a revert commit on {url}, got subject={subject:?}"
            );
        }

        let status: Vec<String> = cap
            .all_messages()
            .into_iter()
            .filter_map(|(lvl, m)| (lvl == LogLevel::Status).then_some(m))
            .collect();
        assert!(
            status
                .iter()
                .any(|m| m == "scoop rollback reverted 2 bucket(s), 0 failure(s)"),
            "expected the canonical summary line, got: {status:?}"
        );
    }

    /// Empty (deduped) evidence emits the canonical empty-evidence warn —
    /// naming the publisher and the supplied clone-target noun — and no
    /// summary line, without spawning any clone.
    #[test]
    fn run_token_revert_rollback_empty_evidence_warns() {
        use anodizer_core::log::LogLevel;
        use anodizer_core::publish_evidence::NixTargetSnapshot;
        use anodizer_core::test_helpers::TestContextBuilder;

        let mut ctx = TestContextBuilder::new().build();
        let cap = anodizer_core::log::LogCapture::new();
        ctx.with_log_capture(cap.clone());

        let empty: Vec<NixTargetSnapshot> = Vec::new();
        run_token_revert_rollback(
            &ctx,
            &empty,
            "nix",
            "NIX_PKGS_TOKEN",
            "overlay clone targets",
            "overlay",
        )
        .expect("empty rollback is a no-op success");

        let warns: Vec<String> = cap
            .all_messages()
            .into_iter()
            .filter_map(|(lvl, m)| (lvl == LogLevel::Warn).then_some(m))
            .collect();
        assert_eq!(warns.len(), 1, "exactly one empty-evidence warn: {warns:?}");
        assert!(
            warns[0].contains("nix") && warns[0].contains("overlay clone targets"),
            "warn must name publisher + noun, got: {warns:?}"
        );
        assert_eq!(cap.status_count(), 0, "empty path emits no summary line");
    }

    /// A target carrying no `token_env_var` falls back to the supplied
    /// `default_env_hint`, which must surface in the per-failure warn so the
    /// operator knows which credential to restore.
    #[test]
    fn run_token_revert_rollback_failure_warn_uses_default_env_hint() {
        use anodizer_core::log::LogLevel;
        use anodizer_core::publish_evidence::ScoopTargetSnapshot;
        use anodizer_core::test_helpers::TestContextBuilder;

        let mut ctx = TestContextBuilder::new().build();
        let cap = anodizer_core::log::LogCapture::new();
        ctx.with_log_capture(cap.clone());

        let targets = vec![ScoopTargetSnapshot {
            target: "bad".into(),
            // Local path that does not exist — clone fails, surfacing a
            // per-target failure warn.
            repo_url: "/this/path/must/not/exist/anywhere/zzz.git".into(),
            branch: Some("master".into()),
            token_env_var: None,
        }];

        let err = run_token_revert_rollback(
            &ctx,
            &targets,
            "scoop",
            "SCOOP_BUCKET_TOKEN",
            "bucket clone targets",
            "bucket",
        )
        .expect_err("a per-target git-revert failure must surface as Err so the caller's rollback summary counts it as failed, not rolled-back");
        assert!(
            format!("{err:#}").contains("1 of 1"),
            "error should report the failure count: {err:#}"
        );

        let warns: Vec<String> = cap
            .all_messages()
            .into_iter()
            .filter_map(|(lvl, m)| (lvl == LogLevel::Warn).then_some(m))
            .collect();
        assert!(
            warns.iter().any(|m| m.contains("SCOOP_BUCKET_TOKEN")),
            "default env hint must appear in the failure warn, got: {warns:?}"
        );
    }
}
