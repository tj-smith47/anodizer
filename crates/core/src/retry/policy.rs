use crate::log::StageLogger;
use std::fmt;
use std::time::Duration;

/// Names the operation a retry engine is driving and carries the logger that
/// surfaces per-attempt failures.
///
/// A required parameter on every retry engine — not an optional builder — so
/// a silent retry is unrepresentable: a backoff ladder can sleep for many
/// minutes (10 attempts × 5m cap), and an operator watching a run must be
/// able to tell "waiting on a transient failure" from "hung".
#[derive(Clone, Copy)]
pub struct RetryLog<'a> {
    desc: &'a str,
    log: &'a StageLogger,
}

impl<'a> RetryLog<'a> {
    /// `desc` is a short human description of the operation being retried
    /// (e.g. `"chocolatey push"`, `"mastodon announce"`); it prefixes every
    /// per-attempt warn line.
    pub fn new(desc: &'a str, log: &'a StageLogger) -> Self {
        Self { desc, log }
    }

    /// The operation description supplied at construction.
    pub fn desc(&self) -> &str {
        self.desc
    }

    pub(super) fn warn_retry(
        &self,
        attempt: u32,
        max: u32,
        cause: &dyn fmt::Display,
        delay: Duration,
    ) {
        // Spelled through the tool's one duration format (`45s`, `2m15s`) so a
        // retry line and an adjacent heartbeat line read the same way.
        self.log.warn(&format!(
            "{} attempt {}/{} failed ({}); retrying in {}",
            self.desc,
            attempt,
            max,
            cause,
            crate::progress::format_elapsed(delay)
        ));
    }

    /// Warn that the ladder exhausted its attempts (or wall-clock budget) and is
    /// giving up after `attempts` tries. Paired with the error the engine then
    /// returns: the error names *what* failed, this line records that the
    /// retries themselves are spent so a watcher does not wait for more.
    pub(super) fn warn_giving_up(&self, attempts: u32) {
        self.log.warn(&format!(
            "{} failed after {} attempt(s), giving up",
            self.desc, attempts
        ));
    }

    /// Note (default-visible) that the operation recovered after `attempts`
    /// tries — the transient failure cleared. Only emitted once at least one
    /// retry has happened, so a clean first attempt stays silent.
    pub(super) fn note_succeeded(&self, attempts: u32) {
        // status, not warn: a recovered transient is a positive per-operation
        // result an operator wants at default verbosity, mirroring the
        // rollback/dry-run default events — not a command echo.
        self.log.status(&format!(
            "{} succeeded after {} attempt(s)",
            self.desc, attempts
        )); // status-ok: recovered-after-retry is a per-operation result event
    }
}

/// Retry policy used by `retry_sync` / `retry_async`.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Total attempts, including the first.
    ///
    /// Invariant: must be `>= 1`. The clamp is enforced at two layers so
    /// every construction path is safe:
    ///
    /// 1. [`crate::config::RetryConfig::to_policy`] clamps user YAML
    ///    (`attempts: 0` -> `1`) at the config-surface boundary.
    /// 2. [`retry_sync`] / [`retry_async`] clamp again at the loop boundary
    ///    to protect direct `RetryPolicy { max_attempts: 0, .. }`
    ///    constructions (e.g. test fixtures).
    ///
    /// Callers therefore do NOT need to clamp `max_attempts` again at the
    /// call site.
    pub max_attempts: u32,
    /// Delay before the second attempt (no wait before the first).
    pub base_delay: Duration,
    /// Upper bound on any individual sleep between attempts.
    pub max_delay: Duration,
}

impl RetryPolicy {
    /// Canonical upload policy: 10 attempts, 50ms
    /// base, 30s cap.
    pub const UPLOAD: RetryPolicy = RetryPolicy {
        max_attempts: 10,
        base_delay: Duration::from_millis(50),
        max_delay: Duration::from_secs(30),
    };

    /// Shallow policy for best-effort pre-publish probes: 3 attempts, 200ms
    /// base, 1s cap.
    ///
    /// Pre-publish probes (token `whoami`, registry index GET, GitHub repo
    /// scope, npm duplicate-version) are an advisory warning gate, not a
    /// write that must land. They run sequentially across every configured
    /// publisher, so the production write-ladder (10 attempts / 10s base /
    /// 5m cap) would let a single wedged endpoint stall the gate for tens of
    /// minutes. A shallow bound keeps the probe responsive while still
    /// absorbing a transient blip; the per-request HTTP timeout still bounds
    /// each individual attempt.
    pub const PREFLIGHT: RetryPolicy = RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_millis(200),
        max_delay: Duration::from_secs(1),
    };

    /// Shallow policy for burn-detection guard probes (published-state
    /// registry lookups made before a destructive rollback): 3 attempts, 1s
    /// base, 30s cap.
    ///
    /// A guard consults one registry endpoint per crate/package, and a
    /// multi-crate workspace probes many of them in one pass, so the
    /// production write-ladder (up to ~25 minutes of backoff per operation)
    /// would let a registry outage stall the guard for hours before it can
    /// classify the outcome. A shallow, capped ladder keeps the whole probe
    /// pass bounded while still absorbing a transient blip; the guard's own
    /// fail-closed / fail-open classification handles genuine outages.
    pub const GUARD_PROBE: RetryPolicy = RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_secs(1),
        max_delay: Duration::from_secs(30),
    };

    pub fn delay_for(&self, next_attempt: u32) -> Duration {
        // `next_attempt` is the attempt we're about to run (≥2). The wait
        // before attempt 2 uses base_delay; before attempt 3 uses base_delay*2;
        // i.e. multiplier = 2^(next_attempt - 2).
        let exp = next_attempt.saturating_sub(2);
        let mult = 1u64.checked_shl(exp).unwrap_or(u64::MAX);
        let ms = (self.base_delay.as_millis() as u64).saturating_mul(mult);
        std::cmp::min(Duration::from_millis(ms), self.max_delay)
    }

    /// Raise this policy's `max_attempts` to at least [`IDEMPOTENT_PUT_ATTEMPTS`]
    /// without disturbing its backoff shape, returning the adjusted policy.
    ///
    /// An idempotent PUT/POST to a fixed target (an Artifactory/generic upload,
    /// a GemFury push, a Snap Store upload, a bucket blob PUT, a GitHub asset
    /// upload) lands the same bytes at the same path on every re-issue, so a
    /// transient 5xx/429 or dropped connection must retry a bounded number of
    /// times even when a stateful mode (`--publish-only`) resolves the
    /// configured policy down to `attempts: 1`. The floor is a `max()` — it
    /// only widens the bound for the retriable classes and never lowers an
    /// operator-set higher value. 4xx responses still fast-fail inside the
    /// per-attempt classifier regardless of this floor.
    pub fn with_idempotent_floor(self) -> RetryPolicy {
        self.with_floor(IDEMPOTENT_PUT_ATTEMPTS)
    }

    /// Raise this policy's `max_attempts` to at least `min`, leaving the backoff
    /// shape untouched. A `max()` floor, never a clamp that lowers a higher
    /// operator-set value.
    pub fn with_floor(self, min: u32) -> RetryPolicy {
        RetryPolicy {
            max_attempts: self.max_attempts.max(min),
            ..self
        }
    }

    /// Whether the wait before `next_attempt` would carry total wall-time past
    /// `deadline`. Checked before each backoff so a long registry storm exits
    /// cleanly (the last error is returned, and an idempotent write recovers on
    /// re-run) instead of being SIGKILLed mid-publish by the outer job timeout.
    /// Shared by the sync and async ladders so both bound identically.
    ///
    /// A saturating check: an uncapped policy (`max_delay: Duration::MAX`) can
    /// project a backoff so large that `Instant::now() + delay` would overflow;
    /// an overflowing projection is treated as past any real deadline (the ladder
    /// stops) rather than panicking.
    pub fn budget_exhausted(&self, next_attempt: u32, deadline: std::time::Instant) -> bool {
        match std::time::Instant::now().checked_add(self.delay_for(next_attempt)) {
            Some(projected) => projected > deadline,
            None => true,
        }
    }
}

/// Total attempt floor for an idempotent PUT/POST, single-sourcing the
/// "3 total attempts" guarantee shared by every idempotent-upload publisher
/// (HTTP upload, GemFury, Snapcraft, GitHub asset, blob). Applied via
/// [`RetryPolicy::with_idempotent_floor`] as a `max()` so a stateful mode
/// (`--publish-only`) that resolves `attempts: 1` still keeps a bounded
/// transient retry, while an operator-set higher cap is preserved.
pub const IDEMPOTENT_PUT_ATTEMPTS: u32 = 3;

/// Default wall-clock budget for a retry ladder when `retry.max_elapsed` is not
/// set. Resolved into an absolute deadline by [`crate::Context::retry_deadline`]
/// and threaded into the engine by publishers, so a ladder bounded only by
/// attempt count cannot run unbounded on a slow-but-not-failing endpoint. It is
/// a *default*, not a hard ceiling: an operator raises (or lowers) it with
/// `retry.max_elapsed`, and a caller that threads `None` is still unbounded.
pub const DEFAULT_MAX_ELAPSED: Duration = Duration::from_secs(15 * 60);

/// Wall-clock time slept in retry backoff so far this run, in milliseconds.
///
/// A release runs as one process, so a single process-global accumulator
/// captures every stage's backoff without threading a handle through the many
/// independent per-stage retry loops — several of which sleep via an injected
/// callback that has no path to carry a handle. Parallel upload workers add
/// concurrently through the atomic. Read once at summary time via
/// [`total_retry_backoff`]; the run surfaces it as a `retry_backoff_secs`
/// field and an operator status line.
static RETRY_BACKOFF_MILLIS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Per-scope retry tally (backoff sleeps + summed wait), keyed by the label of
/// the enclosing [`RetryScope`]. Backoff recorded with no active scope lands
/// under [`UNATTRIBUTED_SCOPE`] so the per-scope rows always sum to the global
/// total. A `Mutex` (not a lock-free map) is ample: retry sleeps are seconds
/// apart, so contention among the parallel upload workers is negligible.
static PER_SCOPE_RETRY: std::sync::Mutex<std::collections::BTreeMap<String, ScopeRetry>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// The label backoff is attributed to while a [`RetryScope`] is active. Stages
/// run serially and each installs one scope, so a single global cell suffices:
/// the release stage's parallel upload tasks all read the same constant value
/// ("release") for the stage's duration, and the serial publish loop swaps it
/// per publisher. No task-local is needed because the value never differs
/// between two concurrently-running sleeps.
static CURRENT_SCOPE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Bucket key for backoff recorded outside any [`RetryScope`].
const UNATTRIBUTED_SCOPE: &str = "(unattributed)";

#[derive(Clone, Copy, Default)]
struct ScopeRetry {
    /// Number of backoff sleeps (i.e. retries) recorded against this scope.
    retries: u32,
    /// Summed backoff wait for this scope, in milliseconds.
    backoff_ms: u64,
}

/// RAII scope that attributes every backoff sleep recorded during its lifetime
/// to `name` (a publisher or stage label). Restores the previous scope on drop,
/// so nested/sequential scopes compose. Install one around each publisher's
/// `run` and around a stage's whole retrying section.
#[must_use = "the scope only applies while the guard is alive"]
pub struct RetryScope {
    prev: Option<String>,
}

impl RetryScope {
    /// Enter a retry-attribution scope named `name`.
    pub fn enter(name: impl Into<String>) -> Self {
        let mut cur = CURRENT_SCOPE.lock().unwrap_or_else(|e| e.into_inner());
        let prev = cur.replace(name.into());
        RetryScope { prev }
    }
}

impl Drop for RetryScope {
    fn drop(&mut self) {
        *CURRENT_SCOPE.lock().unwrap_or_else(|e| e.into_inner()) = self.prev.take();
    }
}

/// Record a backoff sleep of `d` against this run's total and the active scope.
/// Callers that sleep for retry should prefer [`sleep_backoff_blocking`] /
/// [`sleep_backoff_async`] (which record and sleep together); use this directly
/// only when the sleep is performed elsewhere (e.g. an injected `sleep`
/// callback in stage-sign).
pub fn record_retry_backoff(d: Duration) {
    let ms = u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
    RETRY_BACKOFF_MILLIS.fetch_add(ms, std::sync::atomic::Ordering::Relaxed);

    let key = CURRENT_SCOPE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_else(|| UNATTRIBUTED_SCOPE.to_string());
    let mut map = PER_SCOPE_RETRY.lock().unwrap_or_else(|e| e.into_inner());
    let entry = map.entry(key).or_default();
    entry.retries = entry.retries.saturating_add(1);
    entry.backoff_ms = entry.backoff_ms.saturating_add(ms);
}

/// Sleep `d` (blocking) and record it as retry backoff.
///
/// A zero `d` is a no-op: it neither sleeps nor records. A caller that owns its
/// own wait (a rate-limit reset probe that already blocked until quota returned)
/// passes `Duration::ZERO` to re-attempt immediately, and such a re-attempt must
/// not inflate the per-scope "backoff sleeps" counter with a sleep that never
/// happened.
pub fn sleep_backoff_blocking(d: Duration) {
    if d.is_zero() {
        return;
    }
    record_retry_backoff(d);
    std::thread::sleep(d);
}

/// Sleep `d` (async) and record it as retry backoff. A zero `d` is a no-op —
/// see [`sleep_backoff_blocking`].
pub async fn sleep_backoff_async(d: Duration) {
    if d.is_zero() {
        return;
    }
    record_retry_backoff(d);
    tokio::time::sleep(d).await;
}

/// Total wall-clock time slept in retry backoff so far this run.
pub fn total_retry_backoff() -> Duration {
    Duration::from_millis(RETRY_BACKOFF_MILLIS.load(std::sync::atomic::Ordering::Relaxed))
}

/// Per-scope retry breakdown so far this run: `(scope, retries, backoff)` per
/// publisher/stage that backed off, sorted by backoff descending (biggest
/// offender first). Their backoff sums to [`total_retry_backoff`].
pub fn retry_scope_breakdown() -> Vec<(String, u32, Duration)> {
    let map = PER_SCOPE_RETRY.lock().unwrap_or_else(|e| e.into_inner());
    let mut rows: Vec<(String, u32, Duration)> = map
        .iter()
        .map(|(k, v)| (k.clone(), v.retries, Duration::from_millis(v.backoff_ms)))
        .collect();
    // Descending by backoff, then name for a stable tie-break.
    rows.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    rows
}
