use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context as _, Result};

use anodizer_core::config::{DockerRetryConfig, RetryConfig};

// One-shot deprecation warning for `docker.retry` / `docker_manifest.retry`.
// Mirrors GR's deprecation marker on `Docker.Retry`. Fires at most once per
// process so a config with N docker pipes doesn't spam the user.
static DOCKER_RETRY_DEPRECATED_WARNED: OnceLock<()> = OnceLock::new();

fn warn_docker_retry_deprecated_once() {
    if DOCKER_RETRY_DEPRECATED_WARNED.set(()).is_ok() {
        tracing::warn!("docker.retry is deprecated; prefer top-level retry config (Project.Retry)");
    }
}

// ---------------------------------------------------------------------------
// parse_duration_string
// ---------------------------------------------------------------------------

/// Parse a human-readable duration string into a [`Duration`].
///
/// Supported suffixes: `ms` (milliseconds), `s` (seconds), `m` (minutes).
/// Examples: `"500ms"`, `"1s"`, `"30s"`, `"2m"`.
///
/// Returns an error if the string is empty, has an unknown suffix, or contains
/// a non-numeric prefix.
pub fn parse_duration_string(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        anyhow::bail!("empty duration string");
    }

    if let Some(n) = s.strip_suffix("ms") {
        let millis: u64 = n
            .parse()
            .with_context(|| format!("invalid milliseconds in duration '{s}'"))?;
        Ok(Duration::from_millis(millis))
    } else if let Some(n) = s.strip_suffix('m') {
        let mins: u64 = n
            .parse()
            .with_context(|| format!("invalid minutes in duration '{s}'"))?;
        Ok(Duration::from_secs(mins * 60))
    } else if let Some(n) = s.strip_suffix('s') {
        let secs: u64 = n
            .parse()
            .with_context(|| format!("invalid seconds in duration '{s}'"))?;
        Ok(Duration::from_secs(secs))
    } else if let Ok(secs) = s.parse::<u64>() {
        // Bare number without suffix — treat as seconds (GoReleaser compat)
        Ok(Duration::from_secs(secs))
    } else {
        anyhow::bail!(
            "unknown duration suffix in '{s}'; expected ms, s, or m (e.g. '500ms', '1s', '2m')"
        );
    }
}

/// Resolve retry parameters with documented precedence.
///
/// Resolution order (matches GR's `Docker.Retry` deprecation handling):
///
/// 1. **`per_pipe`** (`docker.retry` / `docker_manifest.retry`) — when set,
///    wins outright, BUT a one-shot `tracing::warn!` fires informing the user
///    that the per-pipe block is deprecated and they should migrate to the
///    top-level `retry:` block.
/// 2. **`top_level`** (`Project.Retry`) — used when `per_pipe` is absent.
/// 3. **defaults** (10 attempts, 10s base, 5m cap — matching GR
///    `Project.Retry` defaults) — used when neither is set.
///
/// Returns `(attempts, base_delay, max_delay)`.
pub fn resolve_retry_params(
    per_pipe: &Option<DockerRetryConfig>,
    top_level: &Option<RetryConfig>,
) -> Result<(u32, Duration, Option<Duration>)> {
    // Default max_delay of 5 minutes prevents exponential backoff from growing
    // to unreasonably long waits (e.g. 42 minutes at attempt 9 with 10s base).
    let default_max_delay = Some(Duration::from_secs(300));

    if let Some(cfg) = per_pipe {
        // Per-pipe wins — but warn (once) that this surface is deprecated.
        warn_docker_retry_deprecated_once();
        let attempts = cfg.attempts.unwrap_or(10);
        let base_delay = match &cfg.delay {
            Some(d) => parse_duration_string(d)?,
            None => Duration::from_secs(10),
        };
        let max_delay = match &cfg.max_delay {
            Some(d) => Some(parse_duration_string(d)?),
            None => default_max_delay,
        };
        return Ok((attempts, base_delay, max_delay));
    }

    if let Some(cfg) = top_level {
        let policy = cfg.to_policy();
        return Ok((
            policy.max_attempts,
            policy.base_delay,
            Some(policy.max_delay),
        ));
    }

    Ok((10, Duration::from_secs(10), default_max_delay))
}
