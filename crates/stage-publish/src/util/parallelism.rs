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
        log.warn(&format!("{label}: worker thread panicked: {msg}"));
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
                        "{}: revert + push {} ({})",
                        publisher, target.target, target.repo_url
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
                "{publisher}: mutex poisoned by worker panic; reporting counters as-of poison"
            ));
            poisoned.into_inner()
        }
    }
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
        INIT.get_or_init(|| unsafe {
            std::env::set_var("GIT_AUTHOR_NAME", "Anodize Test");
            std::env::set_var("GIT_AUTHOR_EMAIL", "test@anodize.local");
            std::env::set_var("GIT_COMMITTER_NAME", "Anodize Test");
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
            Command::new("git")
                .args(["init", "--bare", "-b", "master"])
                .arg(bare.path())
                .status()
                .unwrap()
                .success()
        );
        for args in [
            vec!["init", "-b", "master"],
            vec!["config", "user.email", "t@example.invalid"],
            vec!["config", "user.name", "T"],
            vec!["config", "commit.gpgsign", "false"],
        ] {
            assert!(
                Command::new("git")
                    .args(&args)
                    .current_dir(work.path())
                    .status()
                    .unwrap()
                    .success()
            );
        }
        std::fs::write(work.path().join("README"), "hi\n").unwrap();
        for args in [vec!["add", "README"], vec!["commit", "-m", "initial"]] {
            assert!(
                Command::new("git")
                    .args(&args)
                    .current_dir(work.path())
                    .status()
                    .unwrap()
                    .success()
            );
        }
        assert!(
            Command::new("git")
                .args(["remote", "add", "origin"])
                .arg(bare.path())
                .current_dir(work.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["push", "-u", "origin", "master"])
                .current_dir(work.path())
                .status()
                .unwrap()
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
                Command::new("git")
                    .args(["clone", "--depth=2", url])
                    .arg(verify.path().join("repo"))
                    .status()
                    .unwrap()
                    .success()
            );
            let out = Command::new("git")
                .args(["log", "-1", "--pretty=%s"])
                .current_dir(verify.path().join("repo"))
                .output()
                .unwrap();
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
}
