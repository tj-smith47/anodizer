use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anodizer_core::config::{HumanDuration, parse_humantime_duration};
use anyhow::{Result, bail};

/// Parse a `--timeout` value with the duration grammar every config duration
/// field uses (`90m`, `3h`, `2d`, `1h30m`, `500ms`, at most one year). A zero
/// timeout is refused, since it would abort the run before it starts.
pub fn parse_timeout(s: &str) -> Result<Duration> {
    let d = parse_humantime_duration(s).map_err(anyhow::Error::msg)?;
    if d.is_zero() {
        bail!("timeout duration must be greater than zero");
    }
    Ok(d)
}

/// Run a closure with a timeout. If the closure does not complete within the
/// given duration, the process exits with code 124 (the conventional timeout
/// exit code, matching GNU `timeout`).
///
/// Implementation: a watchdog thread sleeps until the deadline, then calls
/// `std::process::exit(124)`. The main thread runs the closure synchronously.
/// If the closure finishes before the deadline, the watchdog thread is
/// abandoned (it will be cleaned up when the process exits).
pub fn run_with_timeout<F>(timeout: Duration, f: F) -> Result<()>
where
    F: FnOnce() -> Result<()>,
{
    let completed = Arc::new(AtomicBool::new(false));
    let completed_clone = completed.clone();

    let _watchdog = std::thread::spawn(move || {
        std::thread::sleep(timeout);
        if !completed_clone.load(Ordering::SeqCst) {
            // Leading newline separates the abort line from any in-progress
            // stage output. Themed through the shared `Error:` render so the
            // watchdog line matches every other error in the run, then exit
            // 124 (GNU `timeout` convention).
            eprintln!(
                "\n{}",
                anodizer_core::log::render_error(&format!(
                    "pipeline timed out after {}; aborting. Use --timeout to increase the limit.",
                    HumanDuration(timeout).as_humantime_string()
                ))
            );
            std::process::exit(124);
        }
    });

    let result = f();
    completed.store(true, Ordering::SeqCst);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timeout_takes_the_config_duration_grammar() {
        for (v, secs) in [
            ("5s", 5),
            ("30m", 1800),
            ("2h30m", 9000),
            ("1h30m10s", 5410),
            ("2d", 172_800),
            ("1 h 30 m", 5400),
            ("365d", 31_536_000),
        ] {
            assert_eq!(parse_timeout(v).unwrap(), Duration::from_secs(secs), "{v}");
        }
        assert_eq!(
            parse_timeout("1500ms").unwrap(),
            Duration::from_millis(1500)
        );
    }

    #[test]
    fn a_timeout_past_one_year_is_refused_with_its_value_and_the_cap() {
        let err = parse_timeout("366d").unwrap_err().to_string();
        assert_eq!(
            err,
            "'366d' is longer than the longest duration anodizer accepts, one year (31536000s)"
        );
    }

    #[test]
    fn a_malformed_or_zero_timeout_is_refused() {
        for v in ["", "30", "invalid", "5x", "1.5h"] {
            assert!(parse_timeout(v).is_err(), "{v:?}");
        }
        let err = parse_timeout("0s").unwrap_err();
        assert!(err.to_string().contains("greater than zero"));
    }

    #[test]
    fn test_run_with_timeout_completes_before_deadline() {
        let result = run_with_timeout(Duration::from_secs(5), || Ok(()));
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_with_timeout_propagates_error() {
        let result = run_with_timeout(Duration::from_secs(5), || {
            anyhow::bail!("intentional error");
        });
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "intentional error");
    }

    // The actual timeout/process::exit path is not exercisable in a unit
    // test because it kills the process. The timeout behavior is verified
    // via an integration test that spawns a child process.
}
