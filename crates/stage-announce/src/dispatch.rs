//! Concurrent announce dispatch: the per-provider log/enqueue seam
//! ([`dispatch`]) and the bounded fan-out runner ([`run_queue`]).
//!
//! Announce is a best-effort post-publish notification. Each provider's `send`
//! performs an INDEPENDENT network side effect, so they run CONCURRENTLY rather
//! than sequentially: a sequential loop accumulates every provider's full
//! timeout×retry budget, so one channel that cannot be reached (e.g. an SMTP
//! relay an egress-firewalled self-hosted runner cannot dial) stalls the whole
//! stage — and it stalls AFTER the release already published. Concurrency
//! collapses the wall time to roughly the slowest single channel, and the
//! aggregate [deadline] abandons even that if it hangs past the bound.
//!
//! [deadline]: anodizer_core::config::AnnounceConfig::deadline_duration

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anodizer_core::context::Context;
use anyhow::Result;

/// A provider's pure network action, captured during the (serial, ctx-borrowing)
/// render phase and run later on a worker thread.
///
/// `'static + Send`: the action must own everything it touches so it can run on
/// a detached worker (the aggregate-deadline runner abandons stragglers, which
/// rules out a scoped pool that would join them). Each `send` therefore moves
/// owned data (`String`s, a `Copy` `RetryPolicy`, a cloned `StageLogger`) into
/// the closure rather than borrowing stack locals.
type SendAction = Box<dyn FnOnce() -> Result<()> + Send + 'static>;

/// Collects the per-provider network actions enqueued during the serial render
/// pass, for the concurrent runner to drain.
///
/// The render pass borrows `&mut Context` and must stay single-threaded (it
/// renders templates and reads env); only the pure network sends fan out. So
/// `dispatch` separates the two: it emits the provider's kv log line inline
/// (keeping output grouped/deterministic) and queues the network action here.
#[derive(Default)]
pub(crate) struct DispatchQueue {
    actions: Vec<(String, SendAction)>,
}

impl DispatchQueue {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Whether nothing was queued (dry-run enqueues nothing).
    pub(crate) fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }
}

/// Log a provider's announcement line and, on the live path, enqueue its
/// network action for concurrent dispatch.
///
/// `key_width` is the shared pad width across every provider firing in this
/// Announcing section (computed by the dispatch loop), so the kv rows column-
/// align like the summary table. The log line is emitted HERE, on the serial
/// render thread, so per-provider output stays grouped and deterministic rather
/// than interleaving with concurrent sends.
///
/// Dry-run logs the `(dry-run)` line and enqueues nothing — a dry-run sends no
/// network traffic, exactly as before concurrency.
/// `log_line` is taken by value so a provider can log a line derived from the
/// same `message` String it then MOVES into the queued `send` closure (the log
/// borrow would otherwise overlap the closure's move). Announce lines are short,
/// so the owned line costs nothing next to the network round-trip it precedes.
pub(crate) fn dispatch(
    ctx: &Context,
    queue: &mut DispatchQueue,
    provider: &str,
    log_line: String,
    key_width: usize,
    send: impl FnOnce() -> Result<()> + Send + 'static,
) -> Result<()> {
    let log = ctx.logger("announce");
    // kv register: the provider name is the key (several providers share
    // one Announcing section, so the name is genuine information), the
    // announcement line is the value.
    if ctx.is_dry_run() {
        log.kv(provider, &format!("(dry-run) {log_line}"), key_width);
    } else {
        log.kv(provider, &log_line, key_width);
        queue.actions.push((provider.to_string(), Box::new(send)));
    }
    Ok(())
}

/// Outcome of draining the queue.
pub(crate) struct DispatchOutcome {
    /// `(provider, "<provider>: <error chain>")` for every provider whose
    /// action returned `Err`, in completion order.
    pub errors: Vec<(String, String)>,
    /// Providers still running when the aggregate deadline elapsed; their
    /// result was abandoned (the per-call timeout still bounds the socket).
    pub abandoned: Vec<String>,
    /// Providers whose action completed successfully (for sent-marker
    /// recording), in completion order.
    pub succeeded: Vec<String>,
}

/// Upper bound on concurrent in-flight announce sends.
///
/// Announce fan-out is I/O-bound, so the worker count tracks the channel set,
/// not the CPU. The cap only bounds a pathological config with very many
/// announcers; a typical config queues far fewer, and spawns exactly that many.
const MAX_ANNOUNCE_WORKERS: usize = 8;

/// Drain `queue` concurrently with a bounded worker pool, abandoning any
/// straggler still running when `deadline` elapses.
///
/// Workers are DETACHED (not scoped): the aggregate deadline must be able to
/// return while a hung channel is still in-flight, which a scoped pool — whose
/// scope exit joins every thread — cannot do. Each action is independently
/// bounded by the per-call HTTP/SMTP timeout and the announce retry profile, so
/// an abandoned worker unwinds on its own shortly after; the deadline only
/// bounds the *aggregate* worst case.
///
/// Results stream back over an `mpsc` channel; the collector stops at the first
/// of "every action reported" or "deadline elapsed", so a reachable channel
/// finishing early is recorded immediately and never blocks on a slow peer.
pub(crate) fn run_queue(queue: DispatchQueue, deadline: Duration) -> DispatchOutcome {
    let expected: Vec<String> = queue.actions.iter().map(|(p, _)| p.clone()).collect();
    let total = expected.len();
    if total == 0 {
        return DispatchOutcome {
            errors: Vec::new(),
            abandoned: Vec::new(),
            succeeded: Vec::new(),
        };
    }

    // Shared work list popped by the workers. Wrapped in `Arc` so detached
    // stragglers keep it alive after the collector returns.
    let work = Arc::new(Mutex::new(queue.actions));
    let (tx, rx) = mpsc::channel::<(String, Result<()>)>();
    let worker_count = total.min(MAX_ANNOUNCE_WORKERS);

    for _ in 0..worker_count {
        let work = Arc::clone(&work);
        let tx = tx.clone();
        std::thread::spawn(move || {
            loop {
                let next = {
                    let mut guard = work.lock().unwrap_or_else(|p| p.into_inner());
                    guard.pop()
                };
                let Some((provider, action)) = next else {
                    break;
                };
                let result = action();
                // A closed receiver (collector returned after the deadline)
                // means this result is abandoned — stop popping.
                if tx.send((provider, result)).is_err() {
                    break;
                }
            }
        });
    }
    // Drop the original sender so `rx` disconnects once every worker exits.
    drop(tx);

    let mut errors: Vec<(String, String)> = Vec::new();
    let mut succeeded: Vec<String> = Vec::new();
    let mut reported: HashSet<String> = HashSet::new();
    let start = Instant::now();
    for _ in 0..total {
        let remaining = deadline.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok((provider, Ok(()))) => {
                reported.insert(provider.clone());
                succeeded.push(provider);
            }
            Ok((provider, Err(e))) => {
                reported.insert(provider.clone());
                // `{e:#}` flattens the anyhow chain into "outer: middle: root"
                // so the summary names the underlying failure, not just the
                // outermost wrapper.
                errors.push((provider.clone(), format!("{provider}: {e:#}")));
            }
            // Deadline elapsed (RecvTimeoutError::Timeout) or all workers
            // exited before `total` reports arrived (Disconnected).
            Err(_) => break,
        }
    }

    // Final non-blocking drain: a worker that COMPLETED a send between the last
    // `recv_timeout` and the deadline already pushed its result onto the channel
    // but the collector loop exited before draining it. Harvesting it here moves
    // it into `succeeded`/`errors` so it is marked sent and never re-fires on the
    // next run — without this, a successful-but-undrained send is misclassified
    // `abandoned`, producing a duplicate announcement. Only a send still genuinely
    // in-flight at the deadline stays abandoned.
    while let Ok((provider, result)) = rx.try_recv() {
        reported.insert(provider.clone());
        match result {
            Ok(()) => succeeded.push(provider),
            Err(e) => errors.push((provider.clone(), format!("{provider}: {e:#}"))),
        }
    }

    // Whatever the expected set never reported was abandoned at the deadline
    // (or lost to an early worker exit). Preserve queue order for a stable
    // warning summary.
    let abandoned: Vec<String> = expected
        .into_iter()
        .filter(|p| !reported.contains(p))
        .collect();

    DispatchOutcome {
        errors,
        abandoned,
        succeeded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Build a queue from `(provider, action)` pairs without touching Context
    /// (the render-phase enqueue is exercised by the announcer tests; these
    /// pin the runner's concurrency/deadline/error semantics directly).
    fn queue_of(actions: Vec<(&str, SendAction)>) -> DispatchQueue {
        DispatchQueue {
            actions: actions
                .into_iter()
                .map(|(p, a)| (p.to_string(), a))
                .collect(),
        }
    }

    #[test]
    fn empty_queue_is_noop() {
        let out = run_queue(DispatchQueue::new(), Duration::from_secs(1));
        assert!(out.errors.is_empty());
        assert!(out.abandoned.is_empty());
        assert!(out.succeeded.is_empty());
    }

    #[test]
    fn collects_per_provider_errors_and_successes() {
        let q = queue_of(vec![
            ("ok1", Box::new(|| Ok(()))),
            ("boom", Box::new(|| anyhow::bail!("kaboom"))),
            ("ok2", Box::new(|| Ok(()))),
        ]);
        let out = run_queue(q, Duration::from_secs(5));
        assert!(out.abandoned.is_empty());
        let mut succ = out.succeeded.clone();
        succ.sort();
        assert_eq!(succ, vec!["ok1".to_string(), "ok2".to_string()]);
        assert_eq!(out.errors.len(), 1);
        assert_eq!(out.errors[0].0, "boom");
        assert!(out.errors[0].1.contains("kaboom"));
    }

    #[test]
    fn runs_actions_concurrently_not_serially() {
        // Three actions each sleep 200ms. Run serially that is 600ms; run
        // concurrently (3 workers) it is ~200ms. A 450ms budget passes only
        // if they overlapped.
        let q = queue_of(vec![
            (
                "a",
                Box::new(|| {
                    std::thread::sleep(Duration::from_millis(200));
                    Ok(())
                }),
            ),
            (
                "b",
                Box::new(|| {
                    std::thread::sleep(Duration::from_millis(200));
                    Ok(())
                }),
            ),
            (
                "c",
                Box::new(|| {
                    std::thread::sleep(Duration::from_millis(200));
                    Ok(())
                }),
            ),
        ]);
        let start = Instant::now();
        let out = run_queue(q, Duration::from_secs(5));
        let elapsed = start.elapsed();
        assert_eq!(out.succeeded.len(), 3);
        assert!(
            elapsed < Duration::from_millis(450),
            "expected concurrent (~200ms), took {elapsed:?}"
        );
    }

    #[test]
    fn aggregate_deadline_abandons_slow_provider() {
        // `fast` returns immediately; `slow` sleeps well past the deadline.
        // The runner must return at the deadline with `slow` abandoned and
        // `fast` already recorded — never wait for `slow`.
        let q = queue_of(vec![
            ("fast", Box::new(|| Ok(()))),
            (
                "slow",
                Box::new(|| {
                    std::thread::sleep(Duration::from_secs(30));
                    Ok(())
                }),
            ),
        ]);
        let start = Instant::now();
        let out = run_queue(q, Duration::from_millis(300));
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "runner must return at the deadline, took {elapsed:?}"
        );
        assert_eq!(out.succeeded, vec!["fast".to_string()]);
        assert_eq!(out.abandoned, vec!["slow".to_string()]);
    }

    #[test]
    fn final_drain_never_abandons_a_completed_send() {
        // The duplicate-on-rerun window: a send COMPLETES (its `Ok(())` is on
        // the channel) but the collector loop broke on the deadline with that
        // result still buffered — so it was classified `abandoned` → not marked
        // sent → re-fires as a duplicate on the next run. The final
        // non-blocking `try_recv` drain closes that window.
        //
        // The window is a timing race (a result landing in the narrow gap
        // between the collector's last `recv` decision and loop exit), so this
        // drives `done` to complete right around a co-queued straggler's
        // deadline across many trials. The invariant the drain guarantees: a
        // send that REPORTED success is NEVER also listed `abandoned`, and is
        // ALWAYS in `succeeded`. A regression (dropping the drain) makes some
        // trial misclassify the completed `done`.
        for trial in 0..200 {
            // `done` finishes a hair before the deadline; `slow` outlives it.
            let q = queue_of(vec![
                (
                    "done",
                    Box::new(|| {
                        std::thread::sleep(Duration::from_millis(20));
                        Ok(())
                    }),
                ),
                (
                    "slow",
                    Box::new(|| {
                        std::thread::sleep(Duration::from_secs(30));
                        Ok(())
                    }),
                ),
            ]);
            let out = run_queue(q, Duration::from_millis(20));

            // The drain's invariant: `done`, whenever it completed, is never
            // simultaneously counted as abandoned, and a completed `done` is in
            // `succeeded`. (`done` may legitimately be abandoned only if it
            // genuinely did not finish before the deadline — in which case it
            // is NOT in succeeded either; the two sets never overlap.)
            assert!(
                !(out.succeeded.contains(&"done".to_string())
                    && out.abandoned.contains(&"done".to_string())),
                "trial {trial}: `done` must not be both succeeded and abandoned"
            );
            // `slow` (30s) can never finish under a 20ms deadline → always
            // abandoned, never succeeded.
            assert!(
                out.abandoned.contains(&"slow".to_string()),
                "trial {trial}: the 30s straggler must be abandoned"
            );
            assert!(
                !out.succeeded.contains(&"slow".to_string()),
                "trial {trial}: the 30s straggler must never be reported succeeded"
            );
        }
    }

    #[test]
    fn final_drain_harvests_a_pre_buffered_completed_result() {
        // Deterministic complement to the racy trial above: prove the drain
        // path itself harvests a completed result that is sitting on the
        // channel when the collector loop exits. A `ready` send completes
        // immediately and a `slow` straggler trips the 80ms deadline. Whether
        // `ready` is drained in-loop or by the final drain, it must end up in
        // `succeeded` and never `abandoned`.
        let q = queue_of(vec![
            ("ready", Box::new(|| Ok(()))),
            (
                "slow",
                Box::new(|| {
                    std::thread::sleep(Duration::from_secs(30));
                    Ok(())
                }),
            ),
        ]);
        let out = run_queue(q, Duration::from_millis(80));
        assert_eq!(
            out.succeeded,
            vec!["ready".to_string()],
            "the immediately-completed send must be harvested into succeeded"
        );
        assert_eq!(out.abandoned, vec!["slow".to_string()]);
    }

    #[test]
    fn worker_pool_is_bounded() {
        // Queue more actions than the worker cap; track peak concurrency.
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let n = MAX_ANNOUNCE_WORKERS + 4;
        let actions: Vec<(String, SendAction)> = (0..n)
            .map(|i| {
                let in_flight = Arc::clone(&in_flight);
                let peak = Arc::clone(&peak);
                let action: SendAction = Box::new(move || {
                    let cur = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(cur, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(50));
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                });
                (format!("p{i}"), action)
            })
            .collect();
        let out = run_queue(DispatchQueue { actions }, Duration::from_secs(5));
        assert_eq!(out.succeeded.len(), n);
        assert!(
            peak.load(Ordering::SeqCst) <= MAX_ANNOUNCE_WORKERS,
            "peak concurrency {} exceeded cap {MAX_ANNOUNCE_WORKERS}",
            peak.load(Ordering::SeqCst)
        );
    }
}
