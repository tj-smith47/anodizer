use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

/// Parse a human-readable duration string into a `std::time::Duration`.
///
/// Supported units: `h` (hours), `m` (minutes), `s` (seconds).
/// Compound durations like `2h30m`, `1h30m10s`, or simple ones like `30m` are supported.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty duration string");
    }

    let mut total_secs: u64 = 0;
    let mut current_num = String::new();
    let mut found_any = false;

    for ch in s.chars() {
        if ch.is_ascii_digit() {
            current_num.push(ch);
        } else {
            if current_num.is_empty() {
                bail!("invalid duration: expected a number before '{}'", ch);
            }
            let n: u64 = current_num
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid number in duration: {}", current_num))?;
            current_num.clear();

            match ch {
                'h' => total_secs += n * 3600,
                'm' => total_secs += n * 60,
                's' => total_secs += n,
                _ => bail!("invalid duration unit '{}' (expected h, m, or s)", ch),
            }
            found_any = true;
        }
    }

    // If there are trailing digits with no unit, that's an error
    if !current_num.is_empty() {
        bail!(
            "invalid duration '{}': number {} has no unit (expected h, m, or s)",
            s,
            current_num
        );
    }

    if !found_any {
        bail!("invalid duration '{}'", s);
    }

    if total_secs == 0 {
        bail!("timeout duration must be greater than zero");
    }

    Ok(Duration::from_secs(total_secs))
}

/// Format a `Duration` into a human-readable string like `1h30m10s`.
fn format_duration(d: Duration) -> String {
    let total = d.as_secs();
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    let mut out = String::new();
    if h > 0 {
        out.push_str(&format!("{}h", h));
    }
    if m > 0 {
        out.push_str(&format!("{}m", m));
    }
    if s > 0 || out.is_empty() {
        out.push_str(&format!("{}s", s));
    }
    out
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
    let deadline = Instant::now() + timeout;
    let completed = Arc::new(AtomicBool::new(false));
    let completed_clone = completed.clone();

    let _watchdog = std::thread::spawn(move || {
        let remaining = deadline.saturating_duration_since(Instant::now());
        std::thread::sleep(remaining);
        if !completed_clone.load(Ordering::SeqCst) {
            eprintln!(
                "\nERROR: pipeline timed out after {}; aborting. Use --timeout to increase the limit.",
                format_duration(timeout)
            );
            std::process::exit(124);
        }
    });

    let result = f();
    completed.store(true, Ordering::SeqCst);
    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_duration_seconds() {
        assert_eq!(parse_duration("5s").unwrap(), Duration::from_secs(5));
    }

    #[test]
    fn test_parse_duration_90_seconds() {
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
    }

    #[test]
    fn test_parse_duration_minutes() {
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1800));
    }

    #[test]
    fn test_parse_duration_hours() {
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
    }

    #[test]
    fn test_parse_duration_compound_hm() {
        assert_eq!(parse_duration("2h30m").unwrap(), Duration::from_secs(9000));
    }

    #[test]
    fn test_parse_duration_compound_hms() {
        assert_eq!(
            parse_duration("1h30m10s").unwrap(),
            Duration::from_secs(5410)
        );
    }

    #[test]
    fn test_parse_duration_invalid_no_unit() {
        assert!(parse_duration("30").is_err());
    }

    #[test]
    fn test_parse_duration_invalid_word() {
        assert!(parse_duration("invalid").is_err());
    }

    #[test]
    fn test_parse_duration_empty() {
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn test_parse_duration_invalid_unit() {
        assert!(parse_duration("5x").is_err());
    }

    #[test]
    fn test_parse_duration_zero_rejected() {
        let err = parse_duration("0s").unwrap_err();
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

    // Note: we cannot easily test the actual timeout/process::exit path in a
    // unit test because it kills the process. The timeout behavior is verified
    // via an integration test that spawns a child process.
}
