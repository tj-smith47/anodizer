//! Shared rollback fan-out primitives used by every publisher whose
//! rollback path issues one network/git call per recorded target.
//!
//! Bundle A's artifactory rollback established the cap; promoting it
//! here lets Bundle B (homebrew / scoop / nix / aur) and any future
//! publisher reuse the same operator-friendly limit and the same
//! [`std::thread::scope`] fan-out shape without copy-pasting the loop.
//!
//! See [`crate::util::git_revert`] for the per-target work
//! Bundle B drives through this primitive.

use anodizer_core::log::StageLogger;
use std::sync::{Mutex, MutexGuard};
use std::thread::ScopedJoinHandle;

use super::git_revert::{RevertTarget, run_git_revert_and_push};

/// Acquire a `Mutex` guard, recovering from poison rather than panicking.
///
/// A poisoned lock means a *sibling* worker thread panicked while
/// holding the guard — the data itself is still readable and the
/// rollback counters are a 3-tuple of `usize` with no invariant a panic
/// could have broken. Panicking this worker too would abandon its
/// already-completed network call without updating the count, silently
/// inflating the `failed` bucket reported to the operator.
pub(crate) fn lock_recover<'a, T>(
    m: &'a Mutex<T>,
    log: &StageLogger,
    label: &str,
) -> MutexGuard<'a, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => {
            log.warn(&format!(
                "{label}: mutex poisoned by sibling thread panic; recovering counter state"
            ));
            poisoned.into_inner()
        }
    }
}

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
/// limit. Bundle B's git revert + push pattern is bounded by the
/// user's network and the git remote's per-IP push rate, both of
/// which 4 stays comfortably under.
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

    /// Empty input must return (0, 0) without spawning a scope.
    /// Guards against a panic in `chunks(0)`-like degenerate cases.
    #[test]
    fn run_revert_targets_parallel_handles_empty_slice() {
        let log = StageLogger::new("test", Verbosity::Normal);
        let (ok, err) = run_revert_targets_parallel(&[], "homebrew", Some("X"), &log);
        assert_eq!(ok, 0);
        assert_eq!(err, 0);
    }
}
