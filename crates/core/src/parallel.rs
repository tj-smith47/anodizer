//! Shared bounded-parallelism helper used by stages that run one subprocess
//! per sub-config (makeself, nfpm, snapcraft, flatpak, upx, …).
//!
//! The stages share the same Step 1 / Step 2 / Step 3 shape:
//!
//! 1. **Step 1** (serial, `&mut ctx`): render templates, stage files,
//!    collect a `Vec<Job>` of fully-owned work units.
//! 2. **Step 2** (parallel, bounded by `ctx.options.parallelism`): run one
//!    subprocess per job in `std::thread::scope`.
//! 3. **Step 3** (serial, `&mut ctx`): register the returned artifacts.
//!
//! Before this helper every stage hand-rolled the Step 2 loop —
//! `for chunk in jobs.chunks(n) { thread::scope(|s| …) }` with its own
//! join-unwrap-or-panic handling. The pattern is now shared here so new
//! parallelized stages just write `run_job`.
//!
//! Semantics match the previous hand-rolled loops exactly:
//!
//! - **Bounded concurrency**: at most `parallelism` workers run at once,
//!   enforced by chunking the job list and scoping threads per-chunk.
//! - **Fail-fast within a chunk**: if any worker in a chunk fails, the whole
//!   chunk still runs to completion (threads are already spawned), but the
//!   caller receives the first error and processes no further chunks.
//! - **Panic-safe**: a worker panic becomes an `anyhow::Error` annotated
//!   with `stage_name`, so a panicked thread doesn't leave the pool
//!   deadlocked or drop all other results on the floor.
//! - **Order-preserving**: results are collected in job-submission order, so
//!   downstream artifact registration remains deterministic.

use anyhow::{Result, anyhow};

use crate::log::StageLogger;
use std::sync::{Mutex, MutexGuard};

/// Acquire a `Mutex` guard, recovering from poison rather than panicking.
///
/// A poisoned lock means a sibling worker thread panicked while holding
/// the guard. For the data shapes this helper is used on (counters,
/// `Vec` accumulators), the inner state has no invariant a panic could
/// have broken — the worst case is one partial write missing. Panicking
/// the current worker too would abandon its already-completed network
/// call without updating the count, silently inflating the operator's
/// `failed` bucket.
pub fn lock_recover<'a, T>(m: &'a Mutex<T>, log: &StageLogger, label: &str) -> MutexGuard<'a, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => {
            log.warn(&format!(
                "{label} mutex poisoned by sibling thread panic; recovering state"
            ));
            poisoned.into_inner()
        }
    }
}

/// Translate a `thread::JoinHandle::join` result's panic payload into
/// an `anyhow::Error` tagged with `label`. The two common panic
/// payload shapes (`&'static str` / `String`) are downcast so the
/// surfaced message is readable rather than the opaque `Any`
/// placeholder.
///
/// Accepts `Result<T, Box<dyn Any + Send>>` rather than the handle
/// itself so a single helper covers both [`std::thread::JoinHandle`]
/// and [`std::thread::ScopedJoinHandle`] — both expose `.join()`
/// returning the same `Result` shape.
///
/// Use when the worker returns `T` and the caller wants `Result<T>`
/// so a panic doesn't propagate as a silently-lost result. For
/// workers that already return `Result<T, anyhow::Error>`, prefer
/// [`run_parallel_chunks`] which bakes this in.
pub fn join_panic_to_err<T>(join_result: std::thread::Result<T>, label: &str) -> Result<T> {
    join_result.map_err(|panic_payload| {
        let msg = if let Some(s) = panic_payload.downcast_ref::<&'static str>() {
            (*s).to_string()
        } else if let Some(s) = panic_payload.downcast_ref::<String>() {
            s.clone()
        } else {
            format!("{:?}", panic_payload)
        };
        anyhow!("{label} worker thread panicked: {msg}")
    })
}

/// Run `run_job` across `jobs` with bounded parallelism. Returns the
/// per-job results in submission order.
///
/// `stage_name` is embedded in the panic error message so a crash in one
/// stage is attributable at a glance (`"nfpm worker thread panicked"` vs
/// `"snapcraft worker thread panicked"`).
///
/// `parallelism` is clamped to `>= 1` internally, so callers can pass
/// `ctx.options.parallelism` without pre-clamping.
pub fn run_parallel_chunks<J, T, F>(
    jobs: &[J],
    parallelism: usize,
    stage_name: &'static str,
    run_job: F,
) -> Result<Vec<T>>
where
    J: Sync,
    T: Send,
    F: Fn(&J) -> Result<T> + Sync,
{
    let parallelism = parallelism.max(1);
    let mut results: Vec<T> = Vec::with_capacity(jobs.len());

    for chunk in jobs.chunks(parallelism) {
        let chunk_results: Vec<Result<T>> = std::thread::scope(|s| {
            let handles: Vec<_> = chunk.iter().map(|job| s.spawn(|| run_job(job))).collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| Err(anyhow!("{} worker thread panicked", stage_name)))
                })
                .collect()
        });

        for r in chunk_results {
            results.push(r?);
        }
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn preserves_submission_order() {
        // Even with multi-threaded execution, the returned Vec must mirror
        // the input slice order so downstream artifact registration is
        // deterministic across runs.
        let jobs: Vec<u32> = (0..20).collect();
        let out = run_parallel_chunks(&jobs, 4, "test", |job| Ok(*job * 10)).unwrap();
        assert_eq!(out, (0..20).map(|i| i * 10).collect::<Vec<_>>());
    }

    #[test]
    fn bounded_concurrency() {
        // With parallelism=2 across 10 jobs, no more than 2 workers should
        // be in-flight at once. We observe this via an AtomicUsize peak
        // counter that each worker increments on entry and decrements on
        // exit, with a small sleep to force overlap.
        let jobs: Vec<u32> = (0..10).collect();
        let in_flight = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);

        run_parallel_chunks(&jobs, 2, "test", |_| {
            let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(10));
            in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();

        assert!(
            peak.load(Ordering::SeqCst) <= 2,
            "peak in-flight workers exceeded parallelism bound"
        );
    }

    #[test]
    fn propagates_first_error() {
        // A single failing job should fail the batch. The job index returned
        // in the error payload asserts the failing worker is the one the
        // caller receives (not silently swallowed by a later success).
        let jobs: Vec<u32> = (0..4).collect();
        let result = run_parallel_chunks(&jobs, 2, "test", |job| {
            if *job == 2 {
                Err(anyhow!("job 2 failed"))
            } else {
                Ok(*job)
            }
        });
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("job 2 failed"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn zero_parallelism_clamps_to_one() {
        // `ctx.options.parallelism` can legitimately be 0 (unset) —
        // callers must not need to pre-clamp. Verify the helper runs
        // sequentially in that case rather than spawning 0 threads.
        let jobs: Vec<u32> = (0..3).collect();
        let out = run_parallel_chunks(&jobs, 0, "test", |job| Ok(*job + 1)).unwrap();
        assert_eq!(out, vec![1, 2, 3]);
    }

    #[test]
    fn empty_jobs_returns_empty() {
        let out: Vec<u32> = run_parallel_chunks::<u32, u32, _>(&[], 4, "test", |_| Ok(0)).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn panic_in_worker_becomes_anyhow_error() {
        // A panicking worker must not take down the whole thread::scope
        // silently — we want an attributable error with the stage name.
        let jobs: Vec<u32> = vec![1, 2, 3];
        let result = run_parallel_chunks(&jobs, 2, "explode-stage", |job| -> Result<u32> {
            if *job == 2 {
                panic!("boom");
            }
            Ok(*job)
        });
        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains("explode-stage worker thread panicked"),
            "unexpected error: {}",
            err
        );
    }

    // ---------- lock_recover ----------

    #[test]
    fn lock_recover_returns_inner_when_unpoisoned() {
        // Happy path: an unpoisoned Mutex yields its guard, the helper
        // adds no observable behavior over a bare `.lock().unwrap()`.
        let log = StageLogger::new("test", crate::log::Verbosity::Quiet);
        let m = Mutex::new(0u32);
        {
            let mut g = lock_recover(&m, &log, "test");
            *g = 42;
        }
        assert_eq!(*m.lock().unwrap(), 42);
    }

    #[test]
    fn lock_recover_recovers_from_poison() {
        // A poisoned Mutex (sibling thread panicked while holding the
        // guard) must yield the inner state rather than panicking the
        // recovering thread too.
        let log = StageLogger::new("test", crate::log::Verbosity::Quiet);
        let m = std::sync::Arc::new(Mutex::new(7u32));
        let m_for_thread = std::sync::Arc::clone(&m);
        let h = std::thread::spawn(move || {
            let _g = m_for_thread.lock().unwrap();
            panic!("poison the mutex");
        });
        let _ = h.join();
        assert!(m.is_poisoned(), "test setup: mutex should be poisoned");
        let g = lock_recover(&m, &log, "test");
        assert_eq!(*g, 7);
    }

    // ---------- join_panic_to_err ----------

    #[test]
    fn join_panic_to_err_passes_through_success() {
        let h = std::thread::spawn(|| 42u32);
        let r = join_panic_to_err(h.join(), "worker").unwrap();
        assert_eq!(r, 42);
    }

    #[test]
    fn join_panic_to_err_translates_str_panic() {
        // The most common panic shape in our codebase is `panic!("msg")`
        // which produces a `&'static str` payload — verify the message
        // survives into the surfaced anyhow chain.
        let h = std::thread::spawn(|| -> u32 {
            panic!("kaboom");
        });
        let err = join_panic_to_err(h.join(), "worker").unwrap_err();
        let s = err.to_string();
        assert!(
            s.contains("worker worker thread panicked") && s.contains("kaboom"),
            "unexpected error: {}",
            s
        );
    }

    #[test]
    fn join_panic_to_err_translates_string_panic() {
        // The other common panic shape — `format!()`-derived `String`
        // payloads — must also be downcast rather than printing as `Any`.
        let h = std::thread::spawn(|| -> u32 {
            panic!("{}", String::from("string-panic"));
        });
        let err = join_panic_to_err(h.join(), "worker").unwrap_err();
        assert!(
            err.to_string().contains("string-panic"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn join_panic_to_err_works_on_scoped_handle() {
        // ScopedJoinHandle::join returns the same Result shape as
        // JoinHandle::join — verify a single helper covers both so
        // callers using `std::thread::scope` don't need a second variant.
        let out: Result<u32> = std::thread::scope(|s| {
            let h = s.spawn(|| 99u32);
            join_panic_to_err(h.join(), "scoped")
        });
        assert_eq!(out.unwrap(), 99);
    }
}
