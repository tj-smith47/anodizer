use super::*;
use std::fmt;
use std::ops::ControlFlow;
use std::time::Duration;

/// Retry a synchronous operation according to `policy`.
///
/// `op` returns:
/// - `Ok(T)` on success (no retry).
/// - `Err(ControlFlow::Continue(e))` to retry if attempts remain.
/// - `Err(ControlFlow::Break(e))` to stop immediately (4xx-style fast-fail).
///
/// Returns the last error if all attempts are exhausted.
///
/// This variant is attempt-count-bounded only; a caller that wants a wall-clock
/// budget (a shorter or operator-raised [`DEFAULT_MAX_ELAPSED`]) uses
/// [`retry_sync_deadline`] with the deadline from
/// [`crate::Context::retry_deadline`].
///
/// Every failed attempt that will be retried emits a default-visible warn
/// (`<desc> attempt n/max failed (<cause>); retrying in <delay>`) via `rlog`
/// before the backoff sleep, so a multi-minute ladder is never silent.
pub fn retry_sync<T, E, F>(rlog: RetryLog<'_>, policy: &RetryPolicy, op: F) -> Result<T, E>
where
    E: fmt::Display,
    F: FnMut(u32) -> Result<T, ControlFlow<E, E>>,
{
    retry_sync_deadline(rlog, policy, None, op)
}

/// Like [`retry_sync`], but stops retrying once the next backoff sleep would
/// push total wall-time past `deadline`. On budget exhaustion it returns the
/// last error observed before the budget was hit, so a caller whose write is
/// idempotent recovers on re-run instead of being killed mid-attempt by an
/// outer timeout. `deadline: None` is byte-for-byte the attempt-count-only
/// behavior of [`retry_sync`].
pub fn retry_sync_deadline<T, E, F>(
    rlog: RetryLog<'_>,
    policy: &RetryPolicy,
    deadline: Option<std::time::Instant>,
    mut op: F,
) -> Result<T, E>
where
    E: fmt::Display,
    F: FnMut(u32) -> Result<T, ControlFlow<E, E>>,
{
    retry_steps_sync(rlog, policy.max_attempts, deadline, |attempt| {
        controlflow_to_step(policy, attempt, op(attempt))
    })
}

/// Adapt a [`ControlFlow`]-classified result into a [`RetryStep`], using
/// `policy` for the backoff shape: `Ok` → `Done`, `Break` → `Fail` (fast-fail),
/// `Continue(e)` → `Retry` sleeping `policy.delay_for(attempt + 1)` with `e`'s
/// `Display` as the per-attempt cause. Single-sources the mapping the sync and
/// async [`ControlFlow`] adapters share.
fn controlflow_to_step<T, E: fmt::Display>(
    policy: &RetryPolicy,
    attempt: u32,
    result: Result<T, ControlFlow<E, E>>,
) -> RetryStep<T, E> {
    match result {
        Ok(v) => RetryStep::Done(v),
        Err(ControlFlow::Break(e)) => RetryStep::Fail(e),
        Err(ControlFlow::Continue(e)) => {
            let cause = e.to_string();
            RetryStep::Retry {
                error: e,
                delay: policy.delay_for(attempt + 1),
                cause,
            }
        }
    }
}

/// Retry an asynchronous operation according to `policy`.
///
/// Same semantics as `retry_sync` but awaits `op` and uses `tokio::time::sleep`.
pub async fn retry_async<T, E, F, Fut>(
    rlog: RetryLog<'_>,
    policy: &RetryPolicy,
    op: F,
) -> Result<T, E>
where
    E: fmt::Display,
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Result<T, ControlFlow<E, E>>>,
{
    retry_async_deadline(rlog, policy, None, op).await
}

/// Like [`retry_async`], but stops once the next backoff would push total
/// wall-time past `deadline` — the async counterpart of [`retry_sync_deadline`],
/// so async publishers (release-asset uploads, GitLab/Gitea API calls layered on
/// [`retry_http_async`]) can honor the same [`crate::Context::retry_deadline`]
/// budget. `deadline: None` is byte-for-byte the attempt-count-only behavior.
pub async fn retry_async_deadline<T, E, F, Fut>(
    rlog: RetryLog<'_>,
    policy: &RetryPolicy,
    deadline: Option<std::time::Instant>,
    mut op: F,
) -> Result<T, E>
where
    E: fmt::Display,
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Result<T, ControlFlow<E, E>>>,
{
    retry_steps_async(rlog, policy.max_attempts, deadline, |attempt| {
        let fut = op(attempt);
        async move { controlflow_to_step(policy, attempt, fut.await) }
    })
    .await
}

/// One attempt's outcome for the step-based retry engines
/// ([`retry_steps_sync`] / [`retry_steps_async`]).
///
/// The operation closure owns *both* classification and the backoff duration;
/// the engine owns everything a hand-rolled loop repeatedly gets wrong — the
/// attempt cap, the wall-clock deadline, backoff accounting, and the full
/// warn / giving-up / succeeded log lifecycle. This is the single primitive
/// every retry ladder in the tree routes through: a publisher that needs a
/// bespoke delay (unjittered exponential, a linear `5·attempt` ladder, a
/// rate-limit reset window) or a bespoke classifier (transient-output markers,
/// index-propagation lag, a partial-upload probe) expresses it in the closure
/// instead of re-implementing the loop and drifting from the others.
///
/// The [`ControlFlow`]-based [`retry_sync`] / [`retry_async`] adapters and the
/// HTTP wrappers are themselves thin layers over these engines, so a fixed
/// [`RetryPolicy`] and a caller-owned delay share one loop, one deadline check,
/// and one set of log lines.
pub enum RetryStep<T, E> {
    /// Stop and succeed with this value. When at least one retry preceded it,
    /// the engine emits the recovery ("succeeded after N attempt(s)") line —
    /// so reserve `Done` for a *clean* success the operator wants confirmed.
    Done(T),
    /// Stop and succeed with this value, but suppress the recovery line. For a
    /// terminal outcome that is success-valued yet carries its own narrative —
    /// an idempotent skip, a tolerated degraded disposition (a kept-stale
    /// asset) — where a "succeeded after N attempt(s)" note would contradict
    /// the closure's own log line rather than confirm a recovery.
    DoneQuiet(T),
    /// Stop and fail with this non-retriable error (a 4xx-style fast-fail).
    /// The engine emits no giving-up line: the operation already classified
    /// this as terminal and knows why.
    Fail(E),
    /// A retriable failure. If an attempt and the wall-clock budget both
    /// remain, the engine sleeps `delay` (recorded as run backoff) and re-runs
    /// the closure; otherwise it stops and returns `error`. `cause` is the
    /// compact, human-readable reason rendered in the per-attempt warn line
    /// (e.g. `"status=503"`, `"sparse-index propagation lag"`).
    Retry {
        error: E,
        delay: Duration,
        cause: String,
    },
}

/// Retry a synchronous operation whose closure owns classification and backoff.
///
/// `max_attempts` bounds the attempt count (clamped to ≥1). `deadline`
/// optionally bounds wall-clock time: before each backoff sleep the engine
/// checks whether `now + delay` would pass it and, if so, stops with the last
/// error (an idempotent write then recovers on re-run instead of being killed
/// mid-attempt by an outer timeout). Every retriable failure emits a
/// default-visible warn before its sleep, an exhausted ladder emits a
/// giving-up warn, and a recovery after ≥1 retry emits a succeeded line — the
/// one retry-log lifecycle shared by every ladder.
pub fn retry_steps_sync<T, E, F>(
    rlog: RetryLog<'_>,
    max_attempts: u32,
    deadline: Option<std::time::Instant>,
    mut op: F,
) -> Result<T, E>
where
    F: FnMut(u32) -> RetryStep<T, E>,
{
    let max = max_attempts.max(1);
    let mut attempt: u32 = 1;
    loop {
        match op(attempt) {
            RetryStep::Done(v) => {
                if attempt > 1 {
                    rlog.note_succeeded(attempt);
                }
                return Ok(v);
            }
            RetryStep::DoneQuiet(v) => return Ok(v),
            RetryStep::Fail(e) => return Err(e),
            RetryStep::Retry {
                error,
                delay,
                cause,
            } => {
                if attempt >= max || deadline_exhausted(deadline, delay) {
                    rlog.warn_giving_up(attempt);
                    return Err(error);
                }
                rlog.warn_retry(attempt, max, &cause, delay);
                sleep_backoff_blocking(delay);
            }
        }
        attempt += 1;
    }
}

/// Async counterpart of [`retry_steps_sync`]; sleeps via [`sleep_backoff_async`]
/// so async ladders honor the same deadline and backoff accounting.
pub async fn retry_steps_async<T, E, F, Fut>(
    rlog: RetryLog<'_>,
    max_attempts: u32,
    deadline: Option<std::time::Instant>,
    mut op: F,
) -> Result<T, E>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = RetryStep<T, E>>,
{
    let max = max_attempts.max(1);
    let mut attempt: u32 = 1;
    loop {
        match op(attempt).await {
            RetryStep::Done(v) => {
                if attempt > 1 {
                    rlog.note_succeeded(attempt);
                }
                return Ok(v);
            }
            RetryStep::DoneQuiet(v) => return Ok(v),
            RetryStep::Fail(e) => return Err(e),
            RetryStep::Retry {
                error,
                delay,
                cause,
            } => {
                if attempt >= max || deadline_exhausted(deadline, delay) {
                    rlog.warn_giving_up(attempt);
                    return Err(error);
                }
                rlog.warn_retry(attempt, max, &cause, delay);
                sleep_backoff_async(delay).await;
            }
        }
        attempt += 1;
    }
}

/// Whether sleeping `delay` now would carry total wall-time past `deadline`.
/// A saturating check: a projection that overflows `Instant` is treated as
/// past any real deadline (stop) rather than panicking. `None` deadline is
/// never exhausted (attempt-count-only bound).
fn deadline_exhausted(deadline: Option<std::time::Instant>, delay: Duration) -> bool {
    deadline.is_some_and(|d| {
        std::time::Instant::now()
            .checked_add(delay)
            .is_none_or(|projected| projected > d)
    })
}
