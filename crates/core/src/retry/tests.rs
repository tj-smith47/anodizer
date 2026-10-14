use super::*;
use crate::log::StageLogger;
use std::error::Error as StdError;
use std::fmt;
use std::io;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use crate::test_helpers::{test_logger, test_retry_log as tlog};

#[test]
fn backoff_accumulator_is_monotonic_and_sleep_helper_records() {
    // The accumulator is process-global and other retry tests run
    // concurrently against it, so assert on the DELTA (never smaller than
    // this test's own contribution) rather than an absolute total — a reset
    // would race those tests. `record_retry_backoff` adds without sleeping;
    // `sleep_backoff_blocking` both sleeps the duration and records it.
    let before = total_retry_backoff();
    record_retry_backoff(Duration::from_millis(250));
    assert!(
        total_retry_backoff().saturating_sub(before) >= Duration::from_millis(250),
        "record_retry_backoff must add at least its duration"
    );

    let before_sleep = total_retry_backoff();
    let start = std::time::Instant::now();
    sleep_backoff_blocking(Duration::from_millis(30));
    assert!(
        start.elapsed() >= Duration::from_millis(30),
        "helper must sleep"
    );
    assert!(
        total_retry_backoff().saturating_sub(before_sleep) >= Duration::from_millis(30),
        "sleep_backoff_blocking must record its sleep"
    );
}

#[test]
fn retry_scope_attributes_backoff_to_its_label() {
    // Isolation rests on the unique scope name plus `>=` delta assertions,
    // not on serialization: the scope stack is per-thread, so a sibling test
    // entering its own `RetryScope` on another thread cannot swap this one
    // away, and a uniquely-named key can only grow inside this test's
    // guarded block.
    let scope_name = "test-scope-attributes-2f9c";
    let read = |name: &str| -> (u32, Duration) {
        retry_scope_breakdown()
            .into_iter()
            .find(|(k, _, _)| k == name)
            .map(|(_, r, d)| (r, d))
            .unwrap_or((0, Duration::ZERO))
    };

    let (r0, d0) = read(scope_name);
    {
        let _scope = RetryScope::enter(scope_name);
        record_retry_backoff(Duration::from_millis(40));
        record_retry_backoff(Duration::from_millis(60));
    }
    let (r1, d1) = read(scope_name);
    assert!(r1 >= r0 + 2, "two records must add at least two retries");
    assert!(
        d1.saturating_sub(d0) >= Duration::from_millis(100),
        "scope backoff must sum the recorded sleeps"
    );

    // After the guard drops, backoff falls back to the unattributed bucket,
    // not this scope — so a later record does not grow this scope's tally.
    record_retry_backoff(Duration::from_millis(10));
    assert_eq!(
        read(scope_name).0,
        r1,
        "records outside the scope must not attribute to it"
    );
}

fn fast_policy() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 4,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(5),
    }
}

/// Locks the shallow shape of the best-effort pre-publish probe policy so a
/// future edit cannot silently re-point preflight probes at the production
/// write-ladder (10 attempts / 10s base / 5m cap), which would let one
/// wedged endpoint stall the gate for tens of minutes.
#[test]
fn preflight_policy_is_shallow() {
    let p = RetryPolicy::PREFLIGHT;
    assert_eq!(p.max_attempts, 3);
    assert_eq!(p.base_delay, Duration::from_millis(200));
    assert_eq!(p.max_delay, Duration::from_secs(1));
    // Sub-second base + low cap: the whole probe ladder must stay well
    // under a second of sleeps even when every attempt is exhausted.
    let total_sleep: Duration = (2..=p.max_attempts).map(|n| p.delay_for(n)).sum();
    assert!(
        total_sleep < Duration::from_secs(1),
        "preflight backoff sleeps must stay sub-second, got {total_sleep:?}"
    );
}

/// Locks the shallow shape of the burn-detection guard probe policy so a
/// future edit cannot silently re-point the published-state guards at the
/// production write-ladder, which would let a registry outage stall a
/// multi-crate probe pass for hours before it can fail closed.
#[test]
fn guard_probe_policy_is_shallow_and_capped() {
    let p = RetryPolicy::GUARD_PROBE;
    assert_eq!(p.max_attempts, 3);
    assert_eq!(p.base_delay, Duration::from_secs(1));
    assert_eq!(p.max_delay, Duration::from_secs(30));
    // Every individual sleep must respect the 30s cap, and the whole
    // ladder must stay bounded (worst case: 1s + 2s of backoff).
    for n in 2..=p.max_attempts {
        assert!(p.delay_for(n) <= Duration::from_secs(30));
    }
    let total_sleep: Duration = (2..=p.max_attempts).map(|n| p.delay_for(n)).sum();
    assert!(
        total_sleep <= Duration::from_secs(3),
        "guard probe backoff must stay in seconds, got {total_sleep:?}"
    );
}

#[test]
fn http_status_extracts_status_from_chain() {
    let wrapped = anyhow::Error::new(HttpError::new(std::io::Error::other("boom"), 429))
        .context("outer context");
    assert_eq!(http_status(&wrapped), 429);
}

#[test]
fn http_status_is_zero_without_http_error() {
    let plain = anyhow::anyhow!("not an http error");
    assert_eq!(http_status(&plain), 0);
}

/// The idempotent floor raises a sub-floor cap to [`IDEMPOTENT_PUT_ATTEMPTS`]
/// but never lowers an operator-set higher cap. Fails if the floor constant
/// is reverted to 1 (or the `max()` semantics flip to a clamp).
#[test]
fn idempotent_floor_raises_low_cap_and_preserves_high_cap() {
    let raised = RetryPolicy {
        max_attempts: 1,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(5),
    }
    .with_idempotent_floor();
    assert_eq!(
        raised.max_attempts, IDEMPOTENT_PUT_ATTEMPTS,
        "a single-attempt cap must be raised to the idempotent floor"
    );

    let preserved = RetryPolicy {
        max_attempts: 7,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(5),
    }
    .with_idempotent_floor();
    assert_eq!(
        preserved.max_attempts, 7,
        "an operator-set cap above the floor must be preserved, not lowered"
    );
}

#[test]
fn jitter_returns_base_when_window_rounds_to_zero() {
    // For any duration under 5ns the ±20 % window (`nanos / 5`) floors to
    // 0, so jitter is a no-op and the base is returned unchanged — the
    // early-return guard that avoids a `% 0` panic on tiny delays.
    for n in 0..5u64 {
        let base = Duration::from_nanos(n);
        assert_eq!(
            jitter_duration(base),
            base,
            "sub-5ns base {n} must pass through unjittered"
        );
    }
}

#[test]
fn jitter_stays_within_plus_minus_twenty_percent() {
    // The jittered value never leaves [base*0.8, base*1.2) — the documented
    // window. Uses a duration large enough that `nanos / 5 > 0`.
    let base = Duration::from_millis(100);
    let jittered = jitter_duration(base);
    let lo = base.mul_f64(0.8);
    let hi = base.mul_f64(1.2);
    assert!(
        jittered >= lo && jittered < hi,
        "jittered {jittered:?} outside [{lo:?}, {hi:?})"
    );
}

#[test]
fn jitter_spreads_consecutive_draws_even_with_a_pinned_clock() {
    // The Weyl-sequence XOR guarantees consecutive draws differ even if
    // the wall clock were frozen (the SOURCE_DATE_EPOCH-style failure
    // mode where a constant seed re-synchronizes concurrent retriers).
    // The clock here is real, but the sequence term alone already forces
    // distinct offsets, so all-equal draws would mean the mixing broke.
    let base = Duration::from_millis(100);
    let draws: Vec<Duration> = (0..8).map(|_| jitter_duration(base)).collect();
    assert!(
        draws.windows(2).any(|w| w[0] != w[1]),
        "8 consecutive jitter draws were all identical: {draws:?}"
    );
}

#[test]
fn delay_progression_caps_at_max() {
    let p = RetryPolicy {
        max_attempts: 10,
        base_delay: Duration::from_millis(100),
        max_delay: Duration::from_millis(500),
    };
    assert_eq!(p.delay_for(2), Duration::from_millis(100));
    assert_eq!(p.delay_for(3), Duration::from_millis(200));
    assert_eq!(p.delay_for(4), Duration::from_millis(400));
    assert_eq!(p.delay_for(5), Duration::from_millis(500)); // capped
    assert_eq!(p.delay_for(8), Duration::from_millis(500)); // capped
}

/// A huge `next_attempt` overflows the `2^(n-2)` multiplier; the
/// `checked_shl(..).unwrap_or(u64::MAX)` + `saturating_mul` guards must yield
/// the capped `max_delay` instead of panicking on overflow.
#[test]
fn delay_for_saturates_on_huge_attempt_without_overflow() {
    let p = RetryPolicy {
        max_attempts: 200,
        base_delay: Duration::from_millis(100),
        max_delay: Duration::from_secs(30),
    };
    // exp = 98 → shift overflows u64 → unwrap_or(MAX) → saturating_mul → capped.
    assert_eq!(p.delay_for(100), Duration::from_secs(30));
    // The very first backoff (attempt 2, exp 0) is still the un-multiplied base.
    assert_eq!(p.delay_for(2), Duration::from_millis(100));
}

/// Locks the production upload ladder shape (10 attempts / 50ms base / 30s
/// cap) the same way `preflight`/`guard_probe` are pinned, so a silent edit to
/// the canonical retry policy is caught.
#[test]
fn upload_policy_shape_is_locked() {
    let p = RetryPolicy::UPLOAD;
    assert_eq!(p.max_attempts, 10);
    assert_eq!(p.base_delay, Duration::from_millis(50));
    assert_eq!(p.max_delay, Duration::from_secs(30));
    // Backoff climbs from the 50ms base and never exceeds the 30s cap.
    assert_eq!(p.delay_for(2), Duration::from_millis(50));
    assert_eq!(p.delay_for(3), Duration::from_millis(100));
    for n in 2..=p.max_attempts {
        assert!(p.delay_for(n) <= Duration::from_secs(30));
    }
}

/// `with_floor` is a `max()` that only ever RAISES the attempt cap: a value
/// below the floor is lifted, an operator-set value above it is preserved.
/// Backoff shape (`base_delay` / `max_delay`) is untouched either way.
#[test]
fn with_floor_general_raises_and_preserves() {
    let base = RetryPolicy {
        max_attempts: 2,
        base_delay: Duration::from_millis(50),
        max_delay: Duration::from_secs(30),
    };
    let raised = base.with_floor(5);
    assert_eq!(raised.max_attempts, 5, "a sub-floor cap must be raised");
    // Shape is preserved verbatim.
    assert_eq!(raised.base_delay, base.base_delay);
    assert_eq!(raised.max_delay, base.max_delay);

    // A cap already above the floor is left alone.
    let high = RetryPolicy {
        max_attempts: 9,
        ..base
    };
    assert_eq!(high.with_floor(5).max_attempts, 9);
}

#[test]
fn sync_succeeds_on_first_attempt() {
    let calls = AtomicU32::new(0);
    let result: Result<&str, &str> = retry_sync(tlog(), &fast_policy(), |_| {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok("ok")
    });
    assert_eq!(result, Ok("ok"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn sync_retries_until_success() {
    let calls = AtomicU32::new(0);
    let result: Result<u32, &str> = retry_sync(tlog(), &fast_policy(), |attempt| {
        calls.fetch_add(1, Ordering::SeqCst);
        if attempt < 3 {
            Err(ControlFlow::Continue("transient"))
        } else {
            Ok(attempt)
        }
    });
    assert_eq!(result, Ok(3));
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[test]
fn sync_break_stops_immediately() {
    let calls = AtomicU32::new(0);
    let result: Result<(), &str> = retry_sync(tlog(), &fast_policy(), |_| {
        calls.fetch_add(1, Ordering::SeqCst);
        Err(ControlFlow::Break("fatal"))
    });
    assert_eq!(result, Err("fatal"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn sync_returns_last_error_after_exhaustion() {
    let calls = AtomicU32::new(0);
    let result: Result<(), String> = retry_sync(tlog(), &fast_policy(), |attempt| {
        calls.fetch_add(1, Ordering::SeqCst);
        Err(ControlFlow::Continue(format!("fail {attempt}")))
    });
    assert_eq!(result, Err("fail 4".to_string()));
    assert_eq!(calls.load(Ordering::SeqCst), 4);
}

/// Build a captured logger + a `RetryLog` borrowing it, so the lifecycle
/// tests can assert on the exact warn / status lines the engine emits.
fn captured() -> (StageLogger, crate::log::LogCapture) {
    StageLogger::with_capture("test", crate::log::Verbosity::Normal)
}

const TINY: Duration = Duration::from_millis(1);

#[test]
fn steps_sync_first_try_done_is_silent() {
    let (log, cap) = captured();
    let out: Result<u32, &str> =
        retry_steps_sync(RetryLog::new("op", &log), 4, None, |_| RetryStep::Done(7));
    assert_eq!(out, Ok(7));
    assert_eq!(cap.total_count(), 0, "a clean first attempt must not log");
}

#[test]
fn steps_sync_retry_then_done_emits_succeeded() {
    let (log, cap) = captured();
    let out: Result<u32, &str> = retry_steps_sync(RetryLog::new("op", &log), 5, None, |attempt| {
        if attempt < 3 {
            RetryStep::Retry {
                error: "transient",
                delay: TINY,
                cause: format!("blip {attempt}"),
            }
        } else {
            RetryStep::Done(attempt)
        }
    });
    assert_eq!(out, Ok(3));
    assert_eq!(cap.warn_count(), 2, "one warn per retried attempt");
    assert!(
        cap.all_messages()
            .iter()
            .any(|(lvl, m)| *lvl == crate::log::LogLevel::Status
                && m.contains("op succeeded after 3 attempts")),
        "recovery after retries must emit a succeeded status line: {:?}",
        cap.all_messages()
    );
}

/// A retry description is often built from a rendered value (an artifact
/// path, an image reference), and every retry line prints it.
#[test]
fn every_retry_line_is_masked_through_the_loggers_table() {
    let secret = "hunter2-retry-label-secret";
    let (log, cap) = captured();
    let log = log.with_job_env([("UPLOAD_TOKEN".to_string(), secret.to_string())]);
    let desc = format!("verification of registry/{secret}/app");

    let recovered: Result<u32, &str> =
        retry_steps_sync(RetryLog::new(&desc, &log), 3, None, |attempt| {
            if attempt < 2 {
                RetryStep::Retry {
                    error: "transient",
                    delay: TINY,
                    cause: format!("401 for {secret}"),
                }
            } else {
                RetryStep::Done(attempt)
            }
        });
    assert_eq!(recovered, Ok(2));
    let exhausted: Result<u32, &str> =
        retry_steps_sync(RetryLog::new(&desc, &log), 2, None, |_| RetryStep::Retry {
            error: "transient",
            delay: TINY,
            cause: "blip".into(),
        });
    assert_eq!(exhausted, Err("transient"));

    let logged: Vec<String> = cap.all_messages().into_iter().map(|(_, m)| m).collect();
    for needle in ["attempt 1/3 failed", "succeeded after 2", "giving up"] {
        assert!(
            logged
                .iter()
                .any(|m| m.contains(needle) && m.contains("registry/$UPLOAD_TOKEN/app")),
            "the `{needle}` line must be printed with the value masked: {logged:?}"
        );
    }
    assert!(!logged.iter().any(|m| m.contains(secret)), "{logged:?}");
}

#[test]
fn steps_sync_done_quiet_recovers_without_succeeded_line() {
    // DoneQuiet returns the value like Done, but a recovery after retries
    // must NOT emit the "succeeded after N" note — the closure owns its own
    // resolution narrative (a tolerated skip / degraded disposition).
    let (log, cap) = captured();
    let out: Result<u32, &str> = retry_steps_sync(RetryLog::new("op", &log), 5, None, |attempt| {
        if attempt < 3 {
            RetryStep::Retry {
                error: "transient",
                delay: TINY,
                cause: "blip".into(),
            }
        } else {
            RetryStep::DoneQuiet(attempt)
        }
    });
    assert_eq!(out, Ok(3));
    assert_eq!(cap.warn_count(), 2, "per-attempt warns still fire");
    assert!(
        !cap.all_messages()
            .iter()
            .any(|(_, m)| m.contains("succeeded after")),
        "DoneQuiet must suppress the recovery line: {:?}",
        cap.all_messages()
    );
}

#[test]
fn two_scopes_on_two_threads_each_keep_their_own_backoff() {
    // Backoff attribution used to live in one process-global cell, so the
    // second thread to enter a scope owned the label both threads then
    // recorded under. The durations are far larger than any sibling test's,
    // and distinct in magnitude, so a stolen record is visible in the sum
    // even while the rest of the crate records concurrently.
    const A_MS: u64 = 1_000_000_000;
    const B_MS: u64 = 4_000_000_000;
    let a = "two-thread-scope-a-6d41";
    let b = "two-thread-scope-b-6d41";
    let entered = std::sync::Barrier::new(2);

    std::thread::scope(|s| {
        for (name, ms) in [(a, A_MS), (b, B_MS)] {
            let entered = &entered;
            s.spawn(move || {
                let _guard = RetryScope::enter(name);
                // Both scopes are open before either records, so a global
                // cell has already lost one of the two labels.
                entered.wait();
                record_retry_backoff(Duration::from_millis(ms));
            });
        }
    });

    let read = |name: &str| -> Duration {
        retry_scope_breakdown()
            .into_iter()
            .find(|(k, _, _)| k == name)
            .map(|(_, _, d)| d)
            .unwrap_or(Duration::ZERO)
    };
    let (got_a, got_b) = (read(a), read(b));
    assert!(
        got_a >= Duration::from_millis(A_MS) && got_a < Duration::from_millis(B_MS),
        "scope a must hold its own backoff and not b's, got {got_a:?}"
    );
    assert!(
        got_b >= Duration::from_millis(B_MS) && got_b < Duration::from_millis(A_MS + B_MS),
        "scope b must hold its own backoff and not a's, got {got_b:?}"
    );
}

#[test]
fn zero_delay_retry_is_not_counted_as_a_backoff_sleep() {
    // A caller that owns its own wait (a rate-limit reset probe) passes a
    // zero delay to re-attempt immediately; that must not inflate the
    // per-scope backoff-sleep count with a sleep that never happened.
    let (log, _cap) = captured();
    let scope = "zero-delay-accounting-probe";
    let _guard = RetryScope::enter(scope);
    let out: Result<u32, &str> = retry_steps_sync(RetryLog::new("op", &log), 5, None, |attempt| {
        if attempt < 3 {
            RetryStep::Retry {
                error: "transient",
                delay: Duration::ZERO,
                cause: "inline wait already served".into(),
            }
        } else {
            RetryStep::Done(attempt)
        }
    });
    assert_eq!(out, Ok(3));
    let recorded = retry_scope_breakdown()
        .into_iter()
        .find(|(name, _, _)| name == scope);
    assert!(
        recorded.is_none(),
        "two zero-delay retries must record no backoff sleeps: {recorded:?}"
    );
}

#[test]
fn steps_sync_fail_fast_is_terminal_and_quiet() {
    let (log, cap) = captured();
    let calls = AtomicU32::new(0);
    let out: Result<(), &str> = retry_steps_sync(RetryLog::new("op", &log), 5, None, |_| {
        calls.fetch_add(1, Ordering::SeqCst);
        RetryStep::Fail("fatal")
    });
    assert_eq!(out, Err("fatal"));
    assert_eq!(calls.load(Ordering::SeqCst), 1, "Fail must not retry");
    assert_eq!(
        cap.warn_count(),
        0,
        "a fast-fail owns its own reason; the engine emits no giving-up line"
    );
}

#[test]
fn steps_sync_exhaustion_emits_giving_up() {
    let (log, cap) = captured();
    let calls = AtomicU32::new(0);
    let out: Result<(), String> = retry_steps_sync(RetryLog::new("op", &log), 3, None, |attempt| {
        calls.fetch_add(1, Ordering::SeqCst);
        RetryStep::Retry {
            error: format!("fail {attempt}"),
            delay: TINY,
            cause: "blip".into(),
        }
    });
    assert_eq!(out, Err("fail 3".to_string()));
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert!(
        cap.warn_messages()
            .iter()
            .any(|m| m.contains("op failed after 3 attempts, giving up")),
        "exhausting the ladder must emit a giving-up warn: {:?}",
        cap.warn_messages()
    );
}

#[test]
fn steps_sync_caller_delay_honors_deadline() {
    let (log, _cap) = captured();
    let calls = AtomicU32::new(0);
    // Deadline already elapsed: the caller-owned delay pushes `now + delay`
    // past it on the first classification, so the ladder stops after one op.
    let deadline = std::time::Instant::now();
    let out: Result<(), &str> =
        retry_steps_sync(RetryLog::new("op", &log), 10, Some(deadline), |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            RetryStep::Retry {
                error: "transient",
                delay: Duration::from_secs(10),
                cause: "blip".into(),
            }
        });
    assert_eq!(out, Err("transient"));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a delay that overshoots the deadline stops after one attempt"
    );
}

#[tokio::test]
async fn steps_async_retry_then_done_emits_succeeded() {
    let (log, cap) = captured();
    let out: Result<u32, &str> =
        retry_steps_async(RetryLog::new("op", &log), 5, None, |attempt| async move {
            if attempt < 2 {
                RetryStep::Retry {
                    error: "transient",
                    delay: TINY,
                    cause: "blip".into(),
                }
            } else {
                RetryStep::Done(attempt)
            }
        })
        .await;
    assert_eq!(out, Ok(2));
    assert_eq!(cap.warn_count(), 1);
    assert!(
        cap.all_messages()
            .iter()
            .any(|(lvl, m)| *lvl == crate::log::LogLevel::Status
                && m.contains("op succeeded after 2 attempts"))
    );
}

#[test]
fn deadline_already_elapsed_stops_after_one_attempt_without_sleeping() {
    // A large base_delay proves the pre-attempt sleep is SKIPPED: with a
    // deadline already in the past, the budget check must fire after the
    // first Continue and return before any 10s sleep runs.
    let policy = RetryPolicy {
        max_attempts: 10,
        base_delay: Duration::from_secs(10),
        max_delay: Duration::from_secs(300),
    };
    let deadline = std::time::Instant::now();
    let calls = AtomicU32::new(0);
    let start = std::time::Instant::now();
    let result: Result<(), &str> = retry_sync_deadline(tlog(), &policy, Some(deadline), |_| {
        calls.fetch_add(1, Ordering::SeqCst);
        Err(ControlFlow::Continue("transient"))
    });
    assert_eq!(result, Err("transient"));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "budget-exhausted retry must call op exactly once"
    );
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "deadline check must skip the 10s backoff sleep, took {:?}",
        start.elapsed()
    );
}

#[test]
fn deadline_none_matches_retry_sync_on_success() {
    let calls = AtomicU32::new(0);
    let result: Result<u32, &str> = retry_sync_deadline(tlog(), &fast_policy(), None, |attempt| {
        calls.fetch_add(1, Ordering::SeqCst);
        if attempt < 2 {
            Err(ControlFlow::Continue("transient"))
        } else {
            Ok(attempt)
        }
    });
    assert_eq!(result, Ok(2));
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    let sync_calls = AtomicU32::new(0);
    let sync_result: Result<u32, &str> = retry_sync(tlog(), &fast_policy(), |attempt| {
        sync_calls.fetch_add(1, Ordering::SeqCst);
        if attempt < 2 {
            Err(ControlFlow::Continue("transient"))
        } else {
            Ok(attempt)
        }
    });
    assert_eq!(sync_result, result);
    assert_eq!(sync_calls.load(Ordering::SeqCst), 2);
}

#[test]
fn deadline_far_in_future_does_not_change_behavior() {
    let deadline = std::time::Instant::now() + Duration::from_secs(3600);
    let calls = AtomicU32::new(0);
    let result: Result<u32, &str> =
        retry_sync_deadline(tlog(), &fast_policy(), Some(deadline), |attempt| {
            calls.fetch_add(1, Ordering::SeqCst);
            if attempt < 3 {
                Err(ControlFlow::Continue("transient"))
            } else {
                Ok(attempt)
            }
        });
    assert_eq!(result, Ok(3));
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[test]
fn budget_exhausted_fires_on_a_past_deadline_and_not_a_future_one() {
    let policy = RetryPolicy {
        max_attempts: 10,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(1),
    };
    let now = std::time::Instant::now();
    assert!(policy.budget_exhausted(2, now - Duration::from_secs(1)));
    assert!(!policy.budget_exhausted(2, now + Duration::from_secs(3600)));
}

#[test]
fn budget_exhausted_saturates_instead_of_panicking_on_uncapped_backoff() {
    // An uncapped policy projects a backoff near Duration::MAX; the check must
    // treat the (overflowing) projection as past the deadline, never panic on
    // `Instant + Duration` overflow (the docker/podman `max_delay: MAX` path).
    let policy = RetryPolicy {
        max_attempts: 100,
        base_delay: Duration::from_secs(30),
        max_delay: Duration::MAX,
    };
    let now = std::time::Instant::now();
    assert!(policy.budget_exhausted(64, now + Duration::from_secs(3600)));
}

#[tokio::test]
async fn async_deadline_none_is_unbounded_and_exhausts_by_count() {
    // retry_async keeps the attempt-count-only contract: a None deadline runs
    // every configured attempt regardless of wall-time.
    let policy = RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(1),
    };
    let calls = std::sync::Arc::new(AtomicU32::new(0));
    let calls_inner = calls.clone();
    let result: Result<(), &str> = retry_async(tlog(), &policy, move |_| {
        let c = calls_inner.clone();
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            Err(ControlFlow::Continue("transient"))
        }
    })
    .await;
    assert_eq!(result, Err("transient"));
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn async_deadline_already_elapsed_stops_after_one_attempt() {
    // The async budget check mirrors the sync one: a past deadline stops the
    // ladder after the first Continue without sleeping the 10s backoff.
    let policy = RetryPolicy {
        max_attempts: 10,
        base_delay: Duration::from_secs(10),
        max_delay: Duration::from_secs(300),
    };
    let deadline = std::time::Instant::now();
    let calls = std::sync::Arc::new(AtomicU32::new(0));
    let calls_inner = calls.clone();
    let start = std::time::Instant::now();
    let result: Result<(), &str> =
        retry_async_deadline(tlog(), &policy, Some(deadline), move |_| {
            let c = calls_inner.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err(ControlFlow::Continue("transient"))
            }
        })
        .await;
    assert_eq!(result, Err("transient"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn async_retries_until_success() {
    let calls = std::sync::Arc::new(AtomicU32::new(0));
    let calls_inner = calls.clone();
    let result: Result<u32, &str> = retry_async(tlog(), &fast_policy(), move |attempt| {
        let c = calls_inner.clone();
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            if attempt < 2 {
                Err(ControlFlow::Continue("transient"))
            } else {
                Ok(attempt)
            }
        }
    })
    .await;
    assert_eq!(result, Ok(2));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

// is_network_error / is_retriable / HttpError / Retriable
//
// Network-error classification test cases.

/// Plain string error wrapper used in classification tests.
#[derive(Debug)]
struct StrErr(&'static str);
impl fmt::Display for StrErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}
impl StdError for StrErr {}

#[derive(Debug)]
struct OwnedErr(String);
impl fmt::Display for OwnedErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl StdError for OwnedErr {}

#[test]
fn network_error_substrings_match() {
    for s in [
        "connection reset by peer",
        "network is unreachable",
        "connection closed unexpectedly",
        "connection refused",
        "tls handshake timeout",
        "i/o timeout",
        "CONNECTION RESET",
        "TLS Handshake Timeout",
        "write: broken pipe",
        "net/http: timeout awaiting response headers",
        "context deadline exceeded",
        // DNS-resolution failures across platforms (hyper-util connector
        // reports these via reqwest as `client error (Connect): dns
        // error: <platform tail>`). Every known tail is pinned so a
        // cross-platform CI failure cannot reintroduce the gap.
        "client error (Connect): dns error: failed to lookup address information: Name or service not known",
        "dns error: nodename nor servname provided, or not known",
        "dns error: No such host is known. (os error 11001)",
    ] {
        let e = OwnedErr(s.to_string());
        assert!(is_network_error(&e), "expected network error: {s:?}");
    }
}

#[test]
fn network_error_io_eof_kinds() {
    let e = io::Error::from(io::ErrorKind::UnexpectedEof);
    assert!(is_network_error(&e));

    // A custom-kind io::Error whose Display is "EOF" (rustls / hyper convention).
    let e2 = io::Error::other("EOF");
    assert!(is_network_error(&e2));
}

// Windows-CI regression: connect() on Windows reports transient failures
// as io::Error { kind: TimedOut, message: "operation timed out" }, neither
// of which matched the original EOF-only kind check or the
// needle list. Same shape for the connection-* kinds across platforms —
// pin each branch.

#[test]
fn is_network_error_classifies_io_timedout() {
    let e = io::Error::from(io::ErrorKind::TimedOut);
    assert!(is_network_error(&e));
    assert!(is_retriable(&e));
}

#[test]
fn is_network_error_classifies_io_connection_refused() {
    let e = io::Error::from(io::ErrorKind::ConnectionRefused);
    assert!(is_network_error(&e));
    assert!(is_retriable(&e));
}

#[test]
fn is_network_error_classifies_io_connection_reset() {
    let e = io::Error::from(io::ErrorKind::ConnectionReset);
    assert!(is_network_error(&e));
    assert!(is_retriable(&e));
}

#[test]
fn is_network_error_classifies_io_connection_aborted() {
    let e = io::Error::from(io::ErrorKind::ConnectionAborted);
    assert!(is_network_error(&e));
    assert!(is_retriable(&e));
}

#[test]
fn is_network_error_classifies_io_broken_pipe() {
    let e = io::Error::from(io::ErrorKind::BrokenPipe);
    assert!(is_network_error(&e));
    assert!(is_retriable(&e));
}

#[test]
fn is_network_error_classifies_operation_timed_out_substring() {
    // Simulate a reqwest- or hyper-wrapped error whose io::ErrorKind has
    // been coerced to Other but whose Display still carries the Windows /
    // macOS TimedOut phrasing. Both the substring path and the
    // ErrorKind path must classify this independently.
    let other_kind = io::Error::other("operation timed out");
    assert!(is_network_error(&other_kind));
    assert!(is_retriable(&other_kind));

    let kind_only = io::Error::from(io::ErrorKind::TimedOut);
    assert!(is_network_error(&kind_only));
    assert!(is_retriable(&kind_only));
}

#[test]
fn network_error_wrapped_unexpected_eof() {
    // Wrap an UnexpectedEof in an outer error so chain-walking is exercised.
    #[derive(Debug)]
    struct Wrap(io::Error);
    impl fmt::Display for Wrap {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "read failed")
        }
    }
    impl StdError for Wrap {
        fn source(&self) -> Option<&(dyn StdError + 'static)> {
            Some(&self.0)
        }
    }
    let inner = io::Error::from(io::ErrorKind::UnexpectedEof);
    let outer = Wrap(inner);
    assert!(is_network_error(&outer));
}

#[test]
fn network_error_non_network_strings_reject() {
    for s in [
        "file not found",
        "permission denied",
        "dial tcp: lookup example.com: no such host",
        "",
    ] {
        let e = OwnedErr(s.to_string());
        assert!(!is_network_error(&e), "expected NOT network error: {s:?}");
    }
}

#[test]
fn retriable_opt_nil_passthrough() {
    assert!(!is_retriable_opt(None));
}

#[test]
fn http_error_500_retriable() {
    let e = HttpError::new(StrErr("internal server error"), 500);
    assert!(is_retriable(&e));
}

#[test]
fn http_error_502_503_retriable() {
    for s in [502u16, 503] {
        let e = HttpError::new(StrErr("bad gateway"), s);
        assert!(is_retriable(&e), "status {s} should be retriable");
    }
}

#[test]
fn http_error_429_retriable() {
    let e = HttpError::new(StrErr("rate limited"), 429);
    assert!(is_retriable(&e));
}

#[test]
fn http_error_4xx_not_retriable() {
    for s in [400u16, 401, 403, 404, 422] {
        let e = HttpError::new(StrErr("client err"), s);
        assert!(!is_retriable(&e), "status {s} should NOT be retriable");
    }
}

#[test]
fn http_error_zero_status_routes_via_message() {
    // Status 0 == network-level failure with no response. Retriability
    // falls back to the network-error substring matcher on the inner.
    let net = HttpError::new(StrErr("connection reset"), 0);
    assert!(is_retriable(&net));

    let non_net = HttpError::new(StrErr("dial failed"), 0);
    assert!(!is_retriable(&non_net));
}

/// An error with a cause, for the chain tests: `top`, caused by `leaf`.
fn caused(top: &'static str, leaf: &'static str) -> io::Error {
    let boxed: Box<dyn StdError + Send + Sync> = anyhow::anyhow!(leaf).context(top).into();
    io::Error::other(boxed)
}

#[test]
fn http_error_unwrap_chain_visible() {
    let e = HttpError::new(StrErr("inner"), 503);
    assert_eq!(e.inner().to_string(), "inner");
    assert!(
        e.source().is_none(),
        "the wrapper displays its leaf inner itself, so nothing follows it"
    );
    let links: Vec<String> = error_links(&e).map(|l| l.to_string()).collect();
    assert_eq!(
        links,
        ["inner", "inner"],
        "the classifiers see the wrapper and the inner"
    );
}

#[test]
fn from_response_nil_resp_yields_status_zero() {
    // No response means status 0.
    // Use a concrete `io::Error` since `reqwest::Error` cannot be
    // synthesised in tests; the API accepts any `E: StdError + Send + Sync`.
    let inner = io::Error::other("connect: dial tcp");
    let e = HttpError::from_response(inner, None);
    assert_eq!(e.status, 0);
}

#[test]
fn from_response_unwrap_chain_visible() {
    // The inner error must remain reachable via the StdError chain so
    // is_retriable's network-error matcher can still see the cause.
    let inner = io::Error::other("connection reset by peer");
    let e = HttpError::from_response(inner, None);
    assert!(
        error_links(&e).any(|l| l.downcast_ref::<io::Error>().is_some()),
        "inner error must be reachable through error_links"
    );
    // And classification must walk through to the network-error matcher.
    assert!(is_retriable(&e));
}

#[test]
fn retriable_wrapper_is_retriable() {
    let e = Retriable::new(StrErr("retry me"));
    assert!(is_retriable(&e));
}

#[test]
fn retriable_wrapper_overrides_4xx() {
    // A 422 wrapped in Retriable is still retriable.
    let inner = HttpError::new(StrErr("exists"), 422);
    let outer = Retriable::new(inner);
    assert!(is_retriable(&outer));
}

#[test]
fn retriable_wrapper_unwrap_chain_visible() {
    let e = Retriable::new(caused("top", "leaf"));
    assert_eq!(e.to_string(), "top");
    assert_eq!(
        e.source().map(|c| c.to_string()).as_deref(),
        Some("leaf"),
        "source() continues below the wrapped error"
    );
    assert!(
        error_links(&e).any(|l| l.downcast_ref::<io::Error>().is_some()),
        "the wrapped error itself is reachable through error_links"
    );
}

/// The two transparent wrappers, the site shape the publishers and the HTTP
/// retry helpers build, print every message of the chain exactly once.
#[test]
fn an_http_error_over_a_context_inner_prints_each_message_once() {
    let site = anyhow::Error::new(
        HttpError::new(io::Error::other("probe failed (HTTP 503): busy"), 503)
            .with_retry_after(Some(Duration::from_secs(1))),
    );
    assert_eq!(format!("{site:#}"), "probe failed (HTTP 503): busy");
    assert_eq!(http_status(&site), 503);
    assert_eq!(http_retry_after(&site), Some(Duration::from_secs(1)));

    let nested = anyhow::Error::new(Retriable::new(HttpError::new(caused("top", "leaf"), 502)))
        .context("publish");
    assert_eq!(format!("{nested:#}"), "publish: top: leaf");
    assert_eq!(
        http_status(&nested),
        502,
        "a status under Retriable is still read"
    );
    assert!(is_retriable(nested.as_ref()));
}

/// What the HTTP retry helper returns for a failed response names the
/// failure once.
#[test]
fn the_retry_helpers_error_names_the_failure_once() {
    use crate::test_helpers::responder::spawn_oneshot_http_responder;
    let (addr, _calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 404 Not Found\r\nContent-Length: 4\r\n\r\ngone",
    ]);
    let client = reqwest::blocking::Client::new();
    let err = retry_http_blocking(
        RetryLog::new("probe", crate::test_helpers::test_logger()),
        &fast_policy(),
        SuccessClass::Strict,
        |_| client.get(format!("http://{addr}/")).send(),
        |status, body| format!("probe failed (HTTP {status}): {body}"),
    )
    .expect_err("a 404 fast-fails");
    assert_eq!(
        format!("{err:#}"),
        "probe failed (HTTP 404 Not Found): gone"
    );
}

/// A transport failure that spends the ladder names the call once on every
/// helper: the transport context opens with the description, so the
/// exhausted-attempts context does not repeat it.
#[tokio::test]
async fn a_spent_transport_ladder_names_the_call_once() {
    let addr = crate::test_helpers::refusing_addr::refusing_addr();
    let url = format!("http://{addr}/");
    let policy = RetryPolicy {
        max_attempts: 2,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
    };
    let msg = |status: reqwest::StatusCode, body: &str| format!("probe failed ({status}): {body}");

    let blocking_url = url.clone();
    let (blocking, bytes) = tokio::task::spawn_blocking(move || {
        let client = reqwest::blocking::Client::new();
        let blocking = retry_http_blocking(
            RetryLog::new("probe", test_logger()),
            &policy,
            SuccessClass::Strict,
            |_| client.get(&blocking_url).send(),
            msg,
        )
        .expect_err("nothing listens");
        let bytes = retry_http_blocking_bytes(
            RetryLog::new("probe", test_logger()),
            &policy,
            SuccessClass::Strict,
            |_| client.get(&blocking_url).send(),
            msg,
        )
        .expect_err("nothing listens");
        (blocking, bytes)
    })
    .await
    .expect("blocking helpers ran");
    let client = reqwest::Client::new();
    let not_blocking = retry_http_async(
        RetryLog::new("probe", test_logger()),
        &policy,
        SuccessClass::Strict,
        |_| client.get(&url).send(),
        msg,
    )
    .await
    .expect_err("nothing listens");

    for err in [blocking, bytes, not_blocking] {
        let chain = format!("{err:#}");
        let expected_head = format!(
            "exhausted retry attempts: probe: HTTP transport error: error sending request \
             for url ({url})"
        );
        // What follows the request line is the operating system's connect
        // error, whose wording differs per platform.
        assert_eq!(
            chain.get(..expected_head.len()),
            Some(expected_head.as_str()),
            "{chain}"
        );
        assert_eq!(chain.matches("probe").count(), 1, "{chain}");
        assert_eq!(http_status(&err), 0);
    }
}

#[test]
fn plain_error_not_retriable() {
    let e = StrErr("something");
    assert!(!is_retriable(&e));
}

#[test]
fn anyhow_error_threadable() {
    // Ensure is_retriable works through anyhow::Error's deref-to-dyn path
    // (which is the canonical caller form across the codebase).
    let e: anyhow::Error = anyhow::anyhow!("connection refused");
    assert!(is_retriable(e.as_ref()));

    let e2: anyhow::Error = anyhow::anyhow!("permission denied");
    assert!(!is_retriable(e2.as_ref()));
}

#[test]
fn is_retriable_chain_walks_to_http_error() {
    // An anyhow::Error wrapping a concrete HttpError must be classified
    // by walking source(), not by Display alone — the message "outer"
    // gives no hint, the 503 status does.
    let inner = HttpError::new(StrErr("bad gateway"), 503);
    let wrapped: anyhow::Error = anyhow::Error::new(inner).context("publish failed");
    assert!(is_retriable(wrapped.as_ref()));
}

// as_ref vs root_cause
//
// Every consumer of the HTTP retry helpers classifies via
// `is_retriable(err.as_ref())`, which reaches `HttpError` whatever the inner
// error is. `root_cause()` reaches it only while the inner error has no cause
// of its own, which is the production shape (`io::Error::other(String)`); an
// inner with a cause of its own makes `root_cause()` the leaf below the
// wrapper, which carries no status, so a 5xx would read as a fast fail.

#[test]
fn classifier_5xx_via_anyhow_chain_uses_as_ref() {
    let wrapped: anyhow::Error =
        anyhow::Error::new(HttpError::new(std::io::Error::other("503"), 503)).context("publish");
    assert!(
        is_retriable(wrapped.as_ref()),
        "5xx HttpError reached via as_ref() must classify retriable"
    );
    assert!(
        is_retriable(wrapped.root_cause()),
        "an inner with no cause of its own leaves HttpError as the root cause"
    );
}

#[test]
fn classifier_root_cause_misses_a_5xx_whose_inner_has_its_own_cause() {
    let wrapped: anyhow::Error =
        anyhow::Error::new(HttpError::new(caused("503", "upstream said no"), 503))
            .context("publish");
    assert!(is_retriable(wrapped.as_ref()));
    assert!(
        !is_retriable(wrapped.root_cause()),
        "root_cause() reaches the inner error's own cause, which carries no status"
    );
}

#[test]
fn classifier_root_cause_misses_a_429_whose_inner_has_its_own_cause() {
    let wrapped: anyhow::Error =
        anyhow::Error::new(HttpError::new(caused("429", "slow down"), 429)).context("publish");
    assert!(is_retriable(wrapped.as_ref()));
    assert!(!is_retriable(wrapped.root_cause()));
}

// retry_http_blocking behavioural tests
//
// `reqwest::Error` has no public constructor, so the transport-error
// branch is exercised indirectly via per-publisher integration tests
// (which mock at the network layer). The unit tests here drive a tiny
// hand-rolled TCP server so the success / non-success status branches
// run against a real reqwest::blocking::Client end-to-end.

use crate::test_helpers::responder::spawn_oneshot_http_responder;

#[test]
fn retry_http_blocking_success_returns_first_attempt() {
    let (addr, calls) =
        spawn_oneshot_http_responder(vec!["HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"]);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");
    let policy = RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
    };
    let result = retry_http_blocking(
        RetryLog::new("test", test_logger()),
        &policy,
        SuccessClass::Strict,
        |_| client.get(format!("http://{addr}/")).send(),
        |_, _| String::from("should not be called on success"),
    );
    let (status, body) = result.expect("success");
    assert_eq!(status.as_u16(), 200);
    assert_eq!(body, "ok");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "single attempt");
}

#[test]
fn retry_http_blocking_retries_5xx_then_succeeds() {
    let (addr, calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
    ]);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");
    let policy = RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
    };
    let result = retry_http_blocking(
        RetryLog::new("test", test_logger()),
        &policy,
        SuccessClass::Strict,
        |_| client.get(format!("http://{addr}/")).send(),
        |status, body| format!("{status}: {body}"),
    );
    let (status, _) = result.expect("eventually succeeds");
    assert_eq!(status.as_u16(), 200);
    assert_eq!(calls.load(Ordering::SeqCst), 2, "one retry then success");
}

#[test]
fn retry_http_blocking_deadline_past_stops_after_one_attempt() {
    let (addr, calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
    ]);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");
    let policy = RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_secs(10),
        max_delay: Duration::from_secs(300),
    };
    let deadline = std::time::Instant::now();
    let result = retry_http_blocking_deadline(
        RetryLog::new("test", test_logger()),
        &policy,
        Some(deadline),
        SuccessClass::Strict,
        |_| client.get(format!("http://{addr}/")).send(),
        |status, body| format!("{status}: {body}"),
    );
    assert!(result.is_err(), "past deadline must fail on the 503");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "past deadline stops before the second attempt"
    );
}

#[test]
fn retry_http_blocking_4xx_fast_fails_no_retry() {
    let (addr, calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nnot found",
    ]);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");
    let policy = RetryPolicy {
        max_attempts: 5,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
    };
    let result = retry_http_blocking(
        RetryLog::new("myscope", test_logger()),
        &policy,
        SuccessClass::Strict,
        |_| client.get(format!("http://{addr}/")).send(),
        |status, body| format!("custom error: {status} body={body}"),
    );
    let err = result.expect_err("4xx must fast-fail");
    let chain = format!("{err:#}");
    assert!(
        chain.contains("custom error"),
        "error formatter must be invoked on non-success; got: {chain}"
    );
    assert!(chain.contains("404"), "status must be in chain: {chain}");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "4xx must NOT retry (only one connection accepted)"
    );
}

#[test]
fn retry_http_blocking_redirect_class_alters_success_predicate() {
    let (addr, _calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 307 Temporary Redirect\r\nLocation: /next\r\nContent-Length: 0\r\n\r\n",
    ]);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        // Disable redirect-following so the 307 reaches the helper.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client");
    let policy = RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
    };
    let result = retry_http_blocking(
        RetryLog::new("test", test_logger()),
        &policy,
        SuccessClass::AllowRedirects,
        |_| client.get(format!("http://{addr}/")).send(),
        |_, _| String::from("should not be called on 3xx with AllowRedirects"),
    );
    let (status, _) = result.expect("3xx is success under AllowRedirects");
    assert_eq!(status.as_u16(), 307);
}

// retry_http_blocking_bytes behavioural tests

#[test]
fn retry_http_blocking_bytes_preserves_non_utf8_body() {
    // A body with invalid-UTF-8 byte sequences (gzip magic + a bare
    // continuation byte) proves the bytes variant does not run a lossy
    // UTF-8 pass over the success payload — `resp.text()` would silently
    // rewrite these to U+FFFD, corrupting the digest of whatever the
    // caller hashes.
    let body: Vec<u8> = vec![0x1f, 0x8b, 0x08, 0x00, 0x80, 0xff, 0xfe, 0x00];
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    let body_for_thread = body.clone();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body_for_thread.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&body_for_thread);
            let _ = stream.flush();
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    });
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");
    let policy = RetryPolicy {
        max_attempts: 1,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
    };
    let result = retry_http_blocking_bytes(
        RetryLog::new("test", test_logger()),
        &policy,
        SuccessClass::Strict,
        |_| client.get(format!("http://{addr}/")).send(),
        |_, _| String::from("should not be called on success"),
    );
    let (status, bytes) = result.expect("success");
    assert_eq!(status.as_u16(), 200);
    assert_eq!(bytes, body, "binary body must round-trip byte-for-byte");
}

#[test]
fn retry_http_blocking_bytes_4xx_fast_fails_no_retry() {
    let (addr, calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nnot found",
    ]);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");
    let policy = RetryPolicy {
        max_attempts: 5,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
    };
    let result = retry_http_blocking_bytes(
        RetryLog::new("myscope", test_logger()),
        &policy,
        SuccessClass::Strict,
        |_| client.get(format!("http://{addr}/")).send(),
        |status, body| format!("custom error: {status} body={body}"),
    );
    let err = result.expect_err("4xx must fast-fail");
    let chain = format!("{err:#}");
    assert!(
        chain.contains("custom error") && chain.contains("not found"),
        "error formatter must see the (lossily-decoded) error body: {chain}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "4xx must NOT retry (only one connection accepted)"
    );
}

#[test]
fn retry_http_blocking_bytes_retries_5xx_then_succeeds() {
    let (addr, calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
    ]);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");
    let policy = RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
    };
    let result = retry_http_blocking_bytes(
        RetryLog::new("test", test_logger()),
        &policy,
        SuccessClass::Strict,
        |_| client.get(format!("http://{addr}/")).send(),
        |status, body| format!("{status}: {body}"),
    );
    let (status, bytes) = result.expect("eventually succeeds");
    assert_eq!(status.as_u16(), 200);
    assert_eq!(bytes, b"ok");
    assert_eq!(calls.load(Ordering::SeqCst), 2, "one retry then success");
}

/// Under `AllowRedirects` the bytes variant treats a returned 3xx as success
/// (never invoking the error formatter), mirroring the text variant. Pins the
/// `success_class` predicate's redirect arm for the bytes path.
#[test]
fn retry_http_blocking_bytes_redirect_class_is_success() {
    let (addr, _calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 307 Temporary Redirect\r\nLocation: /next\r\nContent-Length: 0\r\n\r\n",
    ]);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        // Disable redirect-following so the 307 reaches the helper.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client");
    let policy = RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
    };
    let result = retry_http_blocking_bytes(
        RetryLog::new("test", test_logger()),
        &policy,
        SuccessClass::AllowRedirects,
        |_| client.get(format!("http://{addr}/")).send(),
        |_, _| String::from("error formatter must not run for a 3xx under AllowRedirects"),
    );
    let (status, _bytes) =
        result.expect("3xx is success under AllowRedirects for the bytes variant");
    assert_eq!(status.as_u16(), 307);
}

/// A transport-layer failure in the bytes variant is classified retriable
/// (`Continue`), so the attempt ladder runs more than once before exhausting.
/// Pins the bytes path's transport-error `is_retriable` branch (the text and
/// async variants have their own transport tests).
#[test]
fn retry_http_blocking_bytes_transport_error_retries_then_fails() {
    let attempts = std::sync::Arc::new(AtomicU32::new(0));
    let attempts_inner = attempts.clone();
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");
    let policy = RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
    };
    let result = retry_http_blocking_bytes(
        RetryLog::new("test-transport-bytes", test_logger()),
        &policy,
        SuccessClass::Strict,
        |_| {
            attempts_inner.fetch_add(1, Ordering::SeqCst);
            client.get(TRANSPORT_FAIL_URL).send()
        },
        |_, _| String::from("non-success branch should not be reached"),
    );
    let err = result.expect_err("transport error must surface as Err");
    assert!(
        attempts.load(Ordering::SeqCst) > 1,
        "transport error must be retried; got {} attempts",
        attempts.load(Ordering::SeqCst)
    );
    let chain = format!("{err:#}");
    assert!(
        chain.contains("test-transport-bytes"),
        "label must surface in error chain; got: {chain}"
    );
}

/// `classify_http_sync` maps a real HTTP outcome to the `ControlFlow` shape
/// `retry_sync` expects: 2xx/3xx → `Ok(resp)`, 5xx → `Continue` (retry), 4xx →
/// `Break` (fast-fail), transport error → `Continue`. Drives each arm through a
/// live loopback responder (and a dead address for the transport arm) so the
/// pure classifier is pinned end-to-end without a public `reqwest::Error` ctor.
#[test]
fn classify_http_sync_maps_status_and_transport_to_controlflow() {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        // Hand 3xx responses to the classifier without following them.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client");

    // 2xx → Ok(resp)
    let (addr, _c) =
        spawn_oneshot_http_responder(vec!["HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"]);
    let ok = classify_http_sync(client.get(format!("http://{addr}/")).send());
    assert!(
        matches!(&ok, Ok(resp) if resp.status().as_u16() == 200),
        "2xx must be Ok(resp)"
    );

    // 3xx → Ok(resp): a redirect is a success outcome for this classifier.
    let (addr, _c) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 301 Moved Permanently\r\nLocation: /x\r\nContent-Length: 0\r\n\r\n",
    ]);
    let redir = classify_http_sync(client.get(format!("http://{addr}/")).send());
    assert!(
        matches!(&redir, Ok(resp) if resp.status().as_u16() == 301),
        "3xx must be Ok(resp)"
    );

    // 5xx → Continue (retriable), status echoed in the error.
    let (addr, _c) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
    ]);
    match classify_http_sync(client.get(format!("http://{addr}/")).send()) {
        Err(ControlFlow::Continue(e)) => {
            assert!(
                format!("{e:#}").contains("503"),
                "5xx error must name the status"
            )
        }
        other => panic!("5xx must be Continue, got {other:?}"),
    }

    // 4xx → Break (fast-fail), status echoed in the error.
    let (addr, _c) =
        spawn_oneshot_http_responder(vec!["HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"]);
    match classify_http_sync(client.get(format!("http://{addr}/")).send()) {
        Err(ControlFlow::Break(e)) => {
            assert!(
                format!("{e:#}").contains("404"),
                "4xx error must name the status"
            )
        }
        other => panic!("4xx must be Break, got {other:?}"),
    }

    // Transport-layer failure (dead host) → Continue (retriable).
    let transport = classify_http_sync(client.get(TRANSPORT_FAIL_URL).send());
    assert!(
        matches!(transport, Err(ControlFlow::Continue(_))),
        "transport error must be Continue"
    );
}

// retry_http_async behavioural tests
//
// Mirrors the blocking suite but drives an async reqwest::Client against
// the same hand-rolled TCP responder (running on a worker thread, so the
// tokio reactor is free to drive the client futures). The transport-error
// arm (Err(reqwest::Error)) is exercised by
// `retry_http_{async,blocking}_transport_error_retries_then_fails` below,
// which bind an ephemeral port, drop the listener, then point the client
// at the now-defunct address.

#[tokio::test]
async fn retry_http_async_success_returns_first_attempt() {
    let (addr, calls) =
        spawn_oneshot_http_responder(vec!["HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"]);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");
    let policy = RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
    };
    let result = retry_http_async(
        RetryLog::new("test", test_logger()),
        &policy,
        SuccessClass::Strict,
        |_| client.get(format!("http://{addr}/")).send(),
        |_, _| String::from("should not be called on success"),
    )
    .await;
    let resp = result.expect("success");
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.expect("body");
    assert_eq!(body, "ok");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "single attempt");
}

#[tokio::test]
async fn retry_http_async_retries_5xx_then_succeeds() {
    let (addr, calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
    ]);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");
    let policy = RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
    };
    let result = retry_http_async(
        RetryLog::new("test", test_logger()),
        &policy,
        SuccessClass::Strict,
        |_| client.get(format!("http://{addr}/")).send(),
        |status, body| format!("{status}: {body}"),
    )
    .await;
    let resp = result.expect("eventually succeeds");
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(calls.load(Ordering::SeqCst), 2, "one retry then success");
}

#[tokio::test]
async fn retry_http_async_4xx_fast_fails_no_retry() {
    let (addr, calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nnot found",
    ]);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");
    let policy = RetryPolicy {
        max_attempts: 5,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
    };
    let result = retry_http_async(
        RetryLog::new("myscope", test_logger()),
        &policy,
        SuccessClass::Strict,
        |_| client.get(format!("http://{addr}/")).send(),
        |status, body| format!("custom error: {status} body={body}"),
    )
    .await;
    let err = result.expect_err("4xx must fast-fail");
    let chain = format!("{err:#}");
    assert!(
        chain.contains("custom error"),
        "error formatter must be invoked on non-success; got: {chain}"
    );
    assert!(chain.contains("404"), "status must be in chain: {chain}");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "4xx must NOT retry (only one connection accepted)"
    );
}

#[tokio::test]
async fn retry_http_async_429_retries_then_succeeds() {
    // 429 (Too Many Requests) is the second retriable class alongside
    // 5xx. Ensures the helper doesn't accidentally fast-fail on rate
    // limits — a regression here would defeat the whole point of
    // wiring retry into release publishers.
    let (addr, calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\n\r\n",
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
    ]);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");
    let policy = RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
    };
    let result = retry_http_async(
        RetryLog::new("test", test_logger()),
        &policy,
        SuccessClass::Strict,
        |_| client.get(format!("http://{addr}/")).send(),
        |status, body| format!("{status}: {body}"),
    )
    .await;
    let resp = result.expect("429 retried then success");
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

// transport-error behavioural tests
//
// The transport-error arm (Err(reqwest::Error): DNS failure, connection
// refused, EOF, TLS handshake failure, etc.) is the single most
// important path to prove: it is the one the helper claims to retry
// and that publishers rely on for resilience against transient network
// blips. The pattern below dials the RFC 2606-reserved `.invalid` TLD,
// which is guaranteed never to resolve, so every attempt fails at the
// DNS-resolution stage in a few milliseconds on Linux, macOS, and
// Windows alike.
//
// Pinned here:
//   1. the helper retries (attempt counter > 1)
//   2. eventually returns an Err with the configured label in the chain
// The outer attempt counter is incremented inside the closure, so it
// sees one bump per attempt regardless of the underlying transport
// outcome.
//
// RFC 2606 (https://datatracker.ietf.org/doc/html/rfc2606) reserves the
// `.invalid` TLD precisely for this purpose; using it removes any
// dependence on OS-level TCP semantics (Windows' kernel can retransmit
// SYN against an unbound loopback port until the connect timeout fires
// rather than refusing synchronously like Linux + macOS do).
const TRANSPORT_FAIL_URL: &str = "http://nonexistent.invalid/";

#[test]
fn retry_http_blocking_transport_error_retries_then_fails() {
    let attempts = std::sync::Arc::new(AtomicU32::new(0));
    let attempts_inner = attempts.clone();
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .expect("client");
    let policy = RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
    };
    let result = retry_http_blocking(
        RetryLog::new("test-transport", test_logger()),
        &policy,
        SuccessClass::Strict,
        |_| {
            attempts_inner.fetch_add(1, Ordering::SeqCst);
            client.get(TRANSPORT_FAIL_URL).send()
        },
        |_, _| String::from("non-success branch should not be reached"),
    );
    let err = result.expect_err("transport error must surface as Err");
    let chain = format!("{err:#}");
    assert!(
        attempts.load(Ordering::SeqCst) > 1,
        "transport error must be retried; got {} attempts; chain={chain}",
        attempts.load(Ordering::SeqCst)
    );
    assert!(
        chain.contains("test-transport"),
        "label must surface in error chain; got: {chain}"
    );
}

#[tokio::test]
async fn retry_http_async_transport_error_retries_then_fails() {
    let attempts = std::sync::Arc::new(AtomicU32::new(0));
    let attempts_inner = attempts.clone();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .expect("client");
    let policy = RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
    };
    let result = retry_http_async(
        RetryLog::new("test-transport-async", test_logger()),
        &policy,
        SuccessClass::Strict,
        |_| {
            attempts_inner.fetch_add(1, Ordering::SeqCst);
            client.get(TRANSPORT_FAIL_URL).send()
        },
        |_, _| String::from("non-success branch should not be reached"),
    )
    .await;
    let err = result.expect_err("transport error must surface as Err");
    assert!(
        attempts.load(Ordering::SeqCst) > 1,
        "transport error must be retried; got {} attempts",
        attempts.load(Ordering::SeqCst)
    );
    let chain = format!("{err:#}");
    assert!(
        chain.contains("test-transport-async"),
        "label must surface in error chain; got: {chain}"
    );
}

// PublisherRetryScope — one budget anchor per publisher invocation

#[test]
fn budget_anchor_is_absent_outside_any_publisher_scope() {
    assert_eq!(
        current_budget_anchor(),
        None,
        "a stage-level caller has no invocation anchor and must anchor at its own call"
    );
}

#[test]
fn publisher_scope_anchor_is_stable_for_the_whole_invocation() {
    let scope = PublisherRetryScope::enter("test-anchor-stable");
    let first = current_budget_anchor().expect("entering a publisher scope anchors the budget");
    std::thread::sleep(Duration::from_millis(20));
    let second = current_budget_anchor().expect("anchor stays installed");
    assert_eq!(
        first, second,
        "the anchor must not advance between seams of one invocation"
    );
    drop(scope);
    assert_eq!(
        current_budget_anchor(),
        None,
        "the guard uninstalls on drop"
    );
}

#[test]
fn nested_publisher_scope_inherits_rather_than_widening_the_budget() {
    // The hole this closes: a publisher helper that creates its own budget
    // hands a wedged remote `retry.max_elapsed` a second time. Nothing
    // reachable from inside an invocation — not even a fresh guard of this
    // same type — may re-anchor.
    let _outer = PublisherRetryScope::enter("test-anchor-outer");
    let outer_anchor = current_budget_anchor().expect("outer anchors");
    std::thread::sleep(Duration::from_millis(20));
    {
        let _inner = PublisherRetryScope::enter("test-anchor-inner");
        assert_eq!(
            current_budget_anchor(),
            Some(outer_anchor),
            "a nested scope must inherit the invocation's anchor, never mint a new one"
        );
    }
    assert_eq!(
        current_budget_anchor(),
        Some(outer_anchor),
        "dropping the nested scope must leave the invocation's anchor intact"
    );
}

#[test]
fn a_distinct_later_invocation_gets_its_own_anchor() {
    // Rollback runs after `run` returned: a genuinely separate invocation,
    // which must get a fresh budget rather than inherit a spent one.
    let first = {
        let _publish = PublisherRetryScope::enter("test-anchor-publish");
        current_budget_anchor().expect("publish anchors")
    };
    std::thread::sleep(Duration::from_millis(20));
    let second = {
        let _rollback = PublisherRetryScope::enter("test-anchor-rollback");
        current_budget_anchor().expect("rollback anchors")
    };
    assert!(
        second > first,
        "a later invocation must anchor at its own start, got {second:?} <= {first:?}"
    );
}

#[test]
fn publisher_scope_anchor_does_not_leak_across_threads() {
    // Two invocations racing (dispatch is serial in production, but the test
    // runner is not) must not strand each other's anchor: a stranded anchor
    // would sit in the past forever and collapse every later retry ladder to
    // a single attempt.
    let outer = PublisherRetryScope::enter("test-anchor-thread-a");
    let anchor = current_budget_anchor().expect("this thread anchors");
    let other = std::thread::spawn(|| {
        assert_eq!(
            current_budget_anchor(),
            None,
            "another thread must not observe this invocation's anchor"
        );
        let _guard = PublisherRetryScope::enter("test-anchor-thread-b");
        current_budget_anchor().expect("the other thread anchors independently")
    })
    .join()
    .expect("thread panicked");
    assert_ne!(other, anchor, "each thread anchors its own invocation");
    assert_eq!(
        current_budget_anchor(),
        Some(anchor),
        "another thread's guard must not disturb this one"
    );
    drop(outer);
    assert_eq!(current_budget_anchor(), None);
}

#[test]
fn a_rate_limited_response_names_the_wait_its_headers_ask_for() {
    use reqwest::header::HeaderMap;
    let mut headers = HeaderMap::new();
    headers.insert("Retry-After", "7".parse().unwrap());
    assert_eq!(
        rate_limit_retry_after(429, &headers),
        Some(Duration::from_secs(7)),
        "Retry-After is delta-seconds"
    );
    assert_eq!(
        rate_limit_retry_after(503, &headers),
        None,
        "only a 429 is read, so no other status becomes retriable through a header"
    );

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    headers.insert("RateLimit-Reset", (now + 120).to_string().parse().unwrap());
    let wait = rate_limit_retry_after(429, &headers).expect("a reset in the future is a wait");
    assert!(
        (Duration::from_secs(118)..=Duration::from_secs(120)).contains(&wait),
        "RateLimit-Reset is a unix timestamp and wins over Retry-After, got {wait:?}"
    );

    headers.insert("RateLimit-Reset", (now - 5).to_string().parse().unwrap());
    assert_eq!(
        rate_limit_retry_after(429, &headers),
        Some(Duration::from_secs(7)),
        "a reset in the past falls back to Retry-After"
    );

    let mut dated = HeaderMap::new();
    dated.insert(
        "Retry-After",
        "Wed, 21 Oct 2026 07:28:00 GMT".parse().unwrap(),
    );
    assert_eq!(
        rate_limit_retry_after(429, &dated),
        None,
        "an HTTP date is not read"
    );
    assert_eq!(rate_limit_retry_after(429, &HeaderMap::new()), None);
}

/// A 429 carrying `Retry-After` is retried after THAT wait, not the policy's
/// backoff, and the retry succeeds.
#[test]
fn a_429_with_retry_after_sleeps_the_asked_wait_then_retries() {
    use crate::test_helpers::responder::spawn_oneshot_http_responder;
    let (addr, calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1\r\nContent-Length: 0\r\n\r\n",
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
    ]);
    let client = reqwest::blocking::Client::new();
    let policy = RetryPolicy {
        max_attempts: 2,
        base_delay: Duration::from_secs(30),
        max_delay: Duration::from_secs(30),
    };
    let started = std::time::Instant::now();
    let (status, body) = retry_http_blocking(
        RetryLog::new("probe", crate::test_helpers::test_logger()),
        &policy,
        SuccessClass::Strict,
        |_| client.get(format!("http://{addr}/")).send(),
        |status, body| format!("probe failed (HTTP {status}): {body}"),
    )
    .expect("the retry after the asked wait succeeds");
    let elapsed = started.elapsed();
    assert_eq!(status.as_u16(), 200);
    assert_eq!(body, "ok");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(
        elapsed >= Duration::from_secs(1) && elapsed < Duration::from_secs(10),
        "slept the header's one second, not the policy's thirty: {elapsed:?}"
    );
}

/// A 429 whose response carries no wait header falls back to the policy's
/// backoff and is retried like any other retriable status.
#[test]
fn a_429_without_a_wait_header_falls_back_to_the_policy_backoff() {
    use crate::test_helpers::responder::spawn_oneshot_http_responder;
    let (addr, calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\n\r\n",
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
    ]);
    let client = reqwest::blocking::Client::new();
    let (status, body) = retry_http_blocking(
        RetryLog::new("probe", crate::test_helpers::test_logger()),
        &fast_policy(),
        SuccessClass::Strict,
        |_| client.get(format!("http://{addr}/")).send(),
        |status, body| format!("probe failed (HTTP {status}): {body}"),
    )
    .expect("the policy retry succeeds");
    assert_eq!(status.as_u16(), 200);
    assert_eq!(body, "ok");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

/// The bytes variant reads the same headers: a `RateLimit-Reset` a few
/// seconds ahead is the wait, and the binary success body comes back
/// untouched.
#[test]
fn a_rate_limit_reset_is_the_wait_on_the_bytes_variant() {
    use crate::test_helpers::responder::spawn_oneshot_http_responder_bytes;
    // The header is whole seconds and `now` is truncated, so one second
    // ahead can already be in the past when a loaded host reads the 429,
    // and the wait then falls back to the policy's thirty seconds.
    let reset = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_secs()
        + 3;
    let mut ok = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n".to_vec();
    ok.extend_from_slice(&[0xff, 0xfe]);
    let (addr, calls) = spawn_oneshot_http_responder_bytes(vec![
        format!("HTTP/1.1 429 Too Many Requests\r\nRateLimit-Reset: {reset}\r\nContent-Length: 0\r\n\r\n")
            .into_bytes(),
        ok,
    ]);
    let client = reqwest::blocking::Client::new();
    let policy = RetryPolicy {
        max_attempts: 2,
        base_delay: Duration::from_secs(30),
        max_delay: Duration::from_secs(30),
    };
    let started = std::time::Instant::now();
    let (status, bytes) = retry_http_blocking_bytes(
        RetryLog::new("probe", crate::test_helpers::test_logger()),
        &policy,
        SuccessClass::Strict,
        |_| client.get(format!("http://{addr}/")).send(),
        |status, body| format!("probe failed (HTTP {status}): {body}"),
    )
    .expect("the retry after the reset succeeds");
    assert_eq!(status.as_u16(), 200);
    assert_eq!(bytes, vec![0xff, 0xfe], "the binary body is not re-encoded");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    let elapsed = started.elapsed();
    // The wait is `reset - floor(now)` in whole seconds. With the reset set
    // to `now + 3`, it is between one and three seconds depending on how far
    // the clock moved before the 429 was read, so one second is the bound
    // that holds on every host, and a wait under it means the header was
    // ignored.
    assert!(
        elapsed >= Duration::from_secs(1),
        "the retry waited for the reset header: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "the wait ends at the reset, well before the policy's thirty seconds: {elapsed:?}"
    );
}

/// The async variant honors the same header.
#[tokio::test]
async fn a_429_with_retry_after_is_honored_on_the_async_variant() {
    use crate::test_helpers::responder::spawn_oneshot_http_responder;
    let (addr, calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1\r\nContent-Length: 0\r\n\r\n",
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
    ]);
    let client = reqwest::Client::new();
    let policy = RetryPolicy {
        max_attempts: 2,
        base_delay: Duration::from_secs(30),
        max_delay: Duration::from_secs(30),
    };
    let started = std::time::Instant::now();
    let resp = retry_http_async(
        RetryLog::new("probe", crate::test_helpers::test_logger()),
        &policy,
        SuccessClass::Strict,
        |_| client.get(format!("http://{addr}/")).send(),
        |status, body| format!("probe failed (HTTP {status}): {body}"),
    )
    .await
    .expect("the retry after the asked wait succeeds");
    let elapsed = started.elapsed();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.text().await.expect("body"), "ok");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(
        elapsed >= Duration::from_secs(1) && elapsed < Duration::from_secs(10),
        "slept the header's one second, not the policy's thirty: {elapsed:?}"
    );
}

/// A wait the deadline cannot hold ends the ladder at once: one attempt, no
/// sleep, the exhausted context on the error.
#[test]
fn a_retry_after_past_the_deadline_gives_up_without_sleeping() {
    use crate::test_helpers::responder::spawn_oneshot_http_responder;
    let (addr, calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 60\r\nContent-Length: 0\r\n\r\n",
    ]);
    let client = reqwest::blocking::Client::new();
    let policy = RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(1),
    };
    let started = std::time::Instant::now();
    let err = retry_http_blocking_deadline(
        RetryLog::new("probe", crate::test_helpers::test_logger()),
        &policy,
        Some(std::time::Instant::now() + Duration::from_secs(2)),
        SuccessClass::Strict,
        |_| client.get(format!("http://{addr}/")).send(),
        |status, body| format!("probe failed (HTTP {status}): {body}"),
    )
    .expect_err("a sixty-second wait does not fit a two-second deadline");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "gave up without sleeping: {:?}",
        started.elapsed()
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(http_status(&err), 429);
    assert!(
        format!("{err:#}").contains("probe: exhausted retry attempts"),
        "a ladder the deadline ended carries the exhausted context: {err:#}"
    );
}

/// A fast fail ran one attempt; it is returned as it came, without the
/// exhausted-attempts context a spent ladder carries.
#[test]
fn a_fast_fail_is_returned_without_the_exhausted_context() {
    use crate::test_helpers::responder::spawn_oneshot_http_responder;
    let (addr, calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 404 Not Found\r\nContent-Length: 4\r\n\r\ngone",
    ]);
    let client = reqwest::blocking::Client::new();
    let err = retry_http_blocking(
        RetryLog::new("probe", crate::test_helpers::test_logger()),
        &fast_policy(),
        SuccessClass::Strict,
        |_| client.get(format!("http://{addr}/")).send(),
        |status, body| format!("probe failed (HTTP {status}): {body}"),
    )
    .expect_err("a 404 fast-fails");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(http_status(&err), 404);
    let chain = format!("{err:#}");
    assert!(
        !chain.contains("exhausted retry attempts"),
        "one attempt is not an exhausted ladder: {chain}"
    );
    assert!(
        chain.contains("probe failed (HTTP 404 Not Found): gone"),
        "{chain}"
    );
}

/// `retry_sync` over an `anyhow::Error` reads the wait an [`HttpError`] in
/// the chain carries, so a closure-built 429 sleeps the server's one second
/// instead of the policy's thirty.
#[test]
fn retry_sync_sleeps_the_wait_an_anyhow_http_error_carries() {
    let policy = RetryPolicy {
        max_attempts: 2,
        base_delay: Duration::from_secs(30),
        max_delay: Duration::from_secs(30),
    };
    let started = std::time::Instant::now();
    let result: Result<u32, anyhow::Error> = retry_sync(tlog(), &policy, |attempt| {
        if attempt == 1 {
            Err(ControlFlow::Continue(
                anyhow::Error::new(
                    HttpError::new(io::Error::other("x"), 429)
                        .with_retry_after(Some(Duration::from_secs(1))),
                )
                .context("rate limited"),
            ))
        } else {
            Ok(attempt)
        }
    });
    let elapsed = started.elapsed();
    assert_eq!(result.expect("the second attempt succeeds"), 2);
    assert!(
        elapsed >= Duration::from_secs(1) && elapsed < Duration::from_secs(10),
        "slept the carried one second, not the policy's thirty: {elapsed:?}"
    );
}

/// What one function body does with the [`HttpError`]s it builds, as the
/// wait-header pin reads it.
#[derive(Debug, PartialEq, Eq)]
enum WaitHeaderShape {
    /// Builds no `HttpError` that could carry a wait, or classifies nothing.
    Outside,
    /// Every `HttpError` it builds carries the wait it read.
    Carries,
    /// Builds an `HttpError` that cannot carry the server's wait.
    Drops { built: usize, carried: usize },
}

/// The argument text of every `call` occurrence in `body`, `call` ending in
/// its opening parenthesis.
fn call_arguments<'a>(body: &'a str, call: &str) -> Vec<&'a str> {
    let mut found = Vec::new();
    let mut rest = body;
    while let Some(at) = rest.find(call) {
        let args = &rest[at + call.len()..];
        let end = argument_list_end(args);
        found.push(&args[..end]);
        rest = &args[end..];
    }
    found
}

/// The byte offset of the parenthesis closing an argument list that starts
/// right after its opening parenthesis; string literals are skipped.
fn argument_list_end(args: &str) -> usize {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in args.char_indices() {
        if in_string {
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' if depth == 0 => return i,
            ')' | ']' | '}' => depth -= 1,
            _ => {}
        }
    }
    args.len()
}

/// The last top-level argument of an argument list.
fn last_argument(args: &str) -> &str {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut start = 0;
    for (i, c) in args.char_indices() {
        if in_string {
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 && !args[i + 1..].trim().is_empty() => start = i + 1,
            _ => {}
        }
    }
    args[start..].trim().trim_end_matches(',').trim()
}

/// Reads one function body for the wait-header pin.
///
/// A body that holds a response and classifies it (builds an `HttpError`
/// from it, or asks `status_is_retriable`) must read the wait headers
/// (`rate_limit_retry_after(`) and hand them to every `HttpError` it builds:
/// each `HttpError::new(` and each `HttpError::from_response(` given
/// `Some(` response. Any other body that builds an `HttpError::new(` with a
/// status that is not the literal `0` must call `.with_retry_after(` too: a
/// helper taking the status as a parameter builds a 429 that would sleep
/// the policy's step instead of the server's wait.
fn wait_header_shape(body: &str) -> WaitHeaderShape {
    let new_args = call_arguments(body, "HttpError::new(");
    let from_response_some = call_arguments(body, "HttpError::from_response(")
        .into_iter()
        .filter(|args| last_argument(args).starts_with("Some("))
        .count();
    let built = new_args.len() + from_response_some;
    let with_status = new_args
        .iter()
        .filter(|args| last_argument(args) != "0")
        .count()
        + from_response_some;
    let carried = body.matches(".with_retry_after(").count();
    let classifies = built > 0 || body.contains("status_is_retriable(");
    let holds_a_response = body.contains(".status()") || body.contains(".headers()");
    if classifies && holds_a_response {
        if body.contains("rate_limit_retry_after(") && carried >= built && built > 0 {
            WaitHeaderShape::Carries
        } else {
            WaitHeaderShape::Drops { built, carried }
        }
    } else if with_status == 0 {
        WaitHeaderShape::Outside
    } else if carried >= with_status {
        WaitHeaderShape::Carries
    } else {
        WaitHeaderShape::Drops {
            built: with_status,
            carried,
        }
    }
}

#[test]
fn a_helper_taking_the_status_as_a_parameter_must_carry_the_wait() {
    let helper = r#"fn fail(msg: String, status: u16) -> anyhow::Error {
        anyhow::Error::new(HttpError::new(std::io::Error::other(msg), status))
    }"#;
    assert_eq!(
        wait_header_shape(helper),
        WaitHeaderShape::Drops {
            built: 1,
            carried: 0
        }
    );
    let carrying = r#"fn fail(msg: String, status: u16, wait: Option<Duration>) -> anyhow::Error {
        anyhow::Error::new(HttpError::new(std::io::Error::other(format!("{msg}, {status}")), status).with_retry_after(wait))
    }"#;
    assert_eq!(wait_header_shape(carrying), WaitHeaderShape::Carries);
    let transport = r#"fn fail(e: reqwest::Error) -> anyhow::Error {
        anyhow::Error::new(HttpError::new(e, 0))
    }"#;
    assert_eq!(wait_header_shape(transport), WaitHeaderShape::Outside);
    let from_response = r#"fn fail(e: reqwest::Error, resp: &reqwest::Response) -> anyhow::Error {
        anyhow::Error::new(HttpError::from_response(e, Some(resp)))
    }"#;
    assert_eq!(
        wait_header_shape(from_response),
        WaitHeaderShape::Drops {
            built: 1,
            carried: 0
        }
    );
    let no_response = r#"fn fail(e: reqwest::Error) -> anyhow::Error {
        anyhow::Error::new(HttpError::from_response(e, None))
    }"#;
    assert_eq!(wait_header_shape(no_response), WaitHeaderShape::Outside);
}

/// Every production site that builds an [`HttpError`] able to carry a 429
/// hands it the wait the server asked for, so the retry adapters sleep that
/// wait there too ([`wait_header_shape`] states the rule per body). A body
/// that retries a 429 through a plain `anyhow!` sleeps the policy's step
/// instead, which is what three publishers did while the announce clients
/// honored the header.
#[test]
fn every_http_error_built_from_a_response_carries_its_wait_headers() {
    use crate::test_helpers::test_sources::{
        function_bodies, production_half, workspace_production_sources,
    };
    let mut converted: Vec<String> = Vec::new();
    let mut unconverted: Vec<String> = Vec::new();
    for source in workspace_production_sources() {
        let path = source.to_string_lossy().replace('\\', "/");
        if path.contains("/crates/core/src/retry/") {
            continue;
        }
        let text = std::fs::read_to_string(&source).expect("read source");
        for body in function_bodies(production_half(&text)) {
            let name = body
                .split_once("fn ")
                .map(|(_, rest)| rest.split(['(', '<']).next().unwrap_or("").trim())
                .unwrap_or("");
            match wait_header_shape(&body) {
                WaitHeaderShape::Outside => {}
                WaitHeaderShape::Carries => converted.push(format!("{path}::{name}")),
                WaitHeaderShape::Drops { built, carried } => {
                    unconverted.push(format!("{path}::{name} (built {built}, carried {carried})"))
                }
            }
        }
    }
    assert!(
        unconverted.is_empty(),
        "these bodies build an HttpError that cannot carry the server's wait: {unconverted:#?}"
    );
    assert_eq!(
        converted.len(),
        12,
        "the population of converted bodies (reddit builds its two errors in one function, \
         gitlab's link upload carries the create failure's wait onto its cleanup error): \
         {converted:#?}"
    );
}
