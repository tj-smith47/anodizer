//! Shared bounded-parallelism helper used by stages that run one subprocess
//! per sub-config (makeself, nfpm, snapcraft, flatpak, upx, …).
//!
//! The stages share the same Phase 1 / Phase 2 / Phase 3 shape:
//!
//! 1. **Phase 1** (serial, `&mut ctx`): render templates, stage files,
//!    collect a `Vec<Job>` of fully-owned work units.
//! 2. **Phase 2** (parallel, bounded by `ctx.options.parallelism`): run one
//!    subprocess per job in `std::thread::scope`.
//! 3. **Phase 3** (serial, `&mut ctx`): register the returned artifacts.
//!
//! Before this helper every stage hand-rolled the Phase 2 loop —
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
}
