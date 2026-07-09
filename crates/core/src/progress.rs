//! Heartbeat / liveness progress for long-running work.
//!
//! At default verbosity anodizer shows a per-artifact result line but nothing
//! *during* a step, so a legitimately slow operation — a large `cargo publish`,
//! a multi-minute asset upload over a slow link, notarytool polling Apple — is
//! visually indistinguishable from a hang. Two heartbeat paths close that gap,
//! both emitting a `still …` line ([`heartbeat_message`]) on a shared cadence:
//!
//! - **Subprocess** waits go through [`crate::run`], whose capture loop runs a
//!   [`run_ticker`] thread. This covers every shelled-out tool — cargo,
//!   notarytool, docker, snapcraft, …
//! - **Pure-async** waits (the octocrab / forge HTTP calls that never spawn a
//!   subprocess) wrap their future in [`with_heartbeat`].
//!
//! Both suppress at verbose (the live subprocess tee is already the progress
//! signal) and when the cadence env is set to `0`.

use std::time::{Duration, Instant};

use crate::config::HumanDuration;
use crate::log::StageLogger;

/// Default quiet period before the first heartbeat, and the cadence between
/// subsequent ones.
pub const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Env override for the heartbeat cadence, in milliseconds. The test hook: the
/// suite sets a few-ms interval so a heartbeat fires without a 30s wait. `0`
/// disables heartbeats entirely; a malformed value degrades to the default
/// (heartbeats are presentation, never worth failing a run over).
pub const HEARTBEAT_INTERVAL_ENV: &str = "ANODIZER_HEARTBEAT_INTERVAL_MS";

/// Resolve the heartbeat cadence — `None` when disabled (`…_MS=0`).
pub fn heartbeat_interval() -> Option<Duration> {
    match std::env::var(HEARTBEAT_INTERVAL_ENV).ok().as_deref() {
        Some(raw) => match raw.trim().parse::<u64>() {
            Ok(0) => None,
            Ok(ms) => Some(Duration::from_millis(ms)),
            Err(_) => Some(DEFAULT_HEARTBEAT_INTERVAL),
        },
        None => Some(DEFAULT_HEARTBEAT_INTERVAL),
    }
}

/// Render `d` for a heartbeat line using the tool's canonical compact duration
/// spelling (`45s`, `2m15s`, `1h5m`) — the same [`HumanDuration`] format config
/// values round-trip through, so a duration reads identically everywhere
/// anodizer prints one.
pub fn format_elapsed(d: Duration) -> String {
    HumanDuration(d).as_humantime_string()
}

/// Render one heartbeat line: `still {action} ({elapsed})`. The single template
/// both drivers print, so the line's shape cannot drift between the subprocess
/// ticker (`action` = `running cargo (aarch64-…)`) and the async combinator
/// (`action` = `uploading foo.tar.gz`).
pub fn heartbeat_message(action: &str, start: Instant) -> String {
    format!("still {action} ({})", format_elapsed(start.elapsed()))
}

/// The active heartbeat cadence for `log`, or `None` when heartbeats are off —
/// suppressed outside Normal verbosity ([`StageLogger::heartbeats_enabled`]) or
/// when [`heartbeat_interval`] is disabled. The single gate both the subprocess
/// ticker and [`with_heartbeat`] consult, so the on/off policy lives in one
/// place rather than being re-derived per driver.
pub fn heartbeat_period(log: &StageLogger) -> Option<Duration> {
    log.heartbeats_enabled().then(heartbeat_interval).flatten()
}

/// Run `on_tick` every `interval` until the paired `Sender` sends or is
/// dropped, then return. The caller spawns this on whatever thread flavor it
/// owns (a scoped thread in [`crate::run`], an owned thread in
/// [`crate::disk::FreeSpaceSampler`]); the sender-drop wake means shutdown is
/// immediate — the ticker never has to wait out a residual interval. Because
/// each emission is followed by a fresh full-interval wait, a scheduler stall
/// can never burst a backlog of ticks: at most one fires per elapsed period.
pub fn run_ticker(
    stop_rx: &std::sync::mpsc::Receiver<()>,
    interval: Duration,
    mut on_tick: impl FnMut(),
) {
    while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = stop_rx.recv_timeout(interval) {
        on_tick();
    }
}

/// Await `fut`, emitting a [`heartbeat_message`] every [`heartbeat_interval`]
/// until it resolves, then return its output.
///
/// For a single opaque future (a forge asset upload) there is no intermediate
/// progress signal, so the heartbeat ticks on pure elapsed time and cancels the
/// instant the future completes — the `select!` drops the timer arm as soon as
/// the `fut` arm wins. The first heartbeat lands one full interval in (the
/// immediate zeroth `interval` tick is consumed before the loop), so a fast
/// upload never prints one. Suppressed at verbose or when the cadence is
/// disabled: the future is simply awaited.
pub async fn with_heartbeat<F, T>(log: &StageLogger, label: &str, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    tokio::pin!(fut);
    let Some(period) = heartbeat_period(log) else {
        return fut.await;
    };
    let start = Instant::now();
    let mut ticker = tokio::time::interval(period);
    // Missed ticks (a busy reactor stalling the poll) must not burst a backlog
    // of heartbeats on resume — one line per real elapsed period is the intent.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // `interval`'s first tick fires immediately; drop it so heartbeat #1 is one
    // full period in, not at t=0.
    ticker.tick().await;
    loop {
        tokio::select! {
            out = &mut fut => return out,
            _ = ticker.tick() => {
                log.heartbeat(&heartbeat_message(label, start));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::env::{EnvGuard, env_mutex};

    #[test]
    fn interval_env_zero_disables_and_malformed_degrades() {
        let _lock = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let _g = EnvGuard::set(HEARTBEAT_INTERVAL_ENV, "0");
        assert_eq!(heartbeat_interval(), None, "0 disables heartbeats");
        let _g = EnvGuard::set(HEARTBEAT_INTERVAL_ENV, "75");
        assert_eq!(heartbeat_interval(), Some(Duration::from_millis(75)));
        let _g = EnvGuard::set(HEARTBEAT_INTERVAL_ENV, "not-a-number");
        assert_eq!(
            heartbeat_interval(),
            Some(DEFAULT_HEARTBEAT_INTERVAL),
            "malformed cadence degrades to the default"
        );
    }

    #[test]
    fn run_ticker_ticks_until_sender_drops() {
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let mut ticks = 0u32;
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            drop(stop_tx);
        });
        run_ticker(&stop_rx, Duration::from_millis(30), || ticks += 1);
        handle.join().expect("dropper thread");
        assert!(
            ticks >= 2,
            "a ~120ms window at a 30ms cadence must tick repeatedly; got {ticks}"
        );
    }

    // The env lock is held across `.await` to serialize the whole test against
    // other env-mutating cases; safe here because the `current_thread` runtime
    // has no other task contending the std Mutex within this test.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn slow_future_emits_heartbeats_fast_future_does_not() {
        let _lock = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let _g = EnvGuard::set(HEARTBEAT_INTERVAL_ENV, "30");

        // A future that resolves before the first cadence prints no heartbeat.
        let (fast_log, fast_cap) = StageLogger::with_capture("t", crate::log::Verbosity::Normal);
        with_heartbeat(&fast_log, "uploading", async {
            tokio::time::sleep(Duration::from_millis(5)).await;
        })
        .await;
        assert_eq!(
            fast_cap.heartbeat_count(),
            0,
            "sub-cadence future must not heartbeat"
        );

        // A future spanning several cadences prints one heartbeat per elapsed
        // interval. Real-time bound: ≥2 over ~200ms at a 30ms cadence, robust to
        // scheduler jitter without asserting an exact count.
        let (slow_log, slow_cap) = StageLogger::with_capture("t", crate::log::Verbosity::Normal);
        with_heartbeat(&slow_log, "uploading", async {
            tokio::time::sleep(Duration::from_millis(200)).await;
        })
        .await;
        assert!(
            slow_cap.heartbeat_count() >= 2,
            "a multi-cadence future must heartbeat repeatedly; got {}",
            slow_cap.heartbeat_count()
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn verbose_suppresses_heartbeats() {
        let _lock = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let _g = EnvGuard::set(HEARTBEAT_INTERVAL_ENV, "30");
        let (log, cap) = StageLogger::with_capture("t", crate::log::Verbosity::Verbose);
        with_heartbeat(&log, "uploading", async {
            tokio::time::sleep(Duration::from_millis(150)).await;
        })
        .await;
        assert_eq!(
            cap.heartbeat_count(),
            0,
            "verbose tee is the progress signal; no heartbeat"
        );
    }
}
