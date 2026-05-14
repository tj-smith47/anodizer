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
use std::sync::Mutex;

use super::git_revert::{RevertTarget, run_git_revert_and_push};

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
                            let mut c = counts.lock().expect("counts lock");
                            c.0 += 1;
                        }
                        Err(err) => {
                            let mut c = counts.lock().expect("counts lock");
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
                let _ = h.join();
            }
        });
    }
    counts.into_inner().expect("counts lock")
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
