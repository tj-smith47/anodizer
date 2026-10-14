use std::sync::OnceLock;
use std::time::Duration;

use anyhow::Result;

use anodizer_core::config::{DockerRetryConfig, RetryConfig};

// One-shot deprecation warning for `docker.retry` / `docker_manifest.retry`.
// Deprecation marker on `Docker.Retry`. Fires at most once per
// process so a config with N docker pipes doesn't spam the user.
static DOCKER_RETRY_DEPRECATED_WARNED: OnceLock<()> = OnceLock::new();

fn warn_docker_retry_deprecated_once() {
    if DOCKER_RETRY_DEPRECATED_WARNED.set(()).is_ok() {
        tracing::warn!("docker.retry is deprecated; prefer top-level retry config (Project.Retry)");
    }
}

/// Parse a `docker.retry` delay with the duration format every config field
/// uses (`500ms`, `10s`, `2m`, `1h30m`, at most one year), plus the bare
/// number this deprecated field has always read as seconds (`10` is `10s`).
pub fn parse_duration_string(s: &str) -> Result<Duration> {
    let trimmed = s.trim_matches([' ', '\t']);
    let parsed = if !trimmed.is_empty() && trimmed.bytes().all(|b| b.is_ascii_digit()) {
        anodizer_core::config::parse_humantime_duration(&format!("{trimmed}s"))
    } else {
        anodizer_core::config::parse_humantime_duration(trimmed)
    };
    parsed.map_err(anyhow::Error::msg)
}

/// Resolve retry parameters with documented precedence.
///
/// Resolution order:
///
/// 1. **`per_pipe`** (`docker.retry` / `docker_manifest.retry`) — when set,
///    wins outright, BUT a one-shot `tracing::warn!` fires informing the user
///    that the per-pipe block is deprecated and they should migrate to the
///    top-level `retry:` block.
/// 2. **`top_level`** (`Project.Retry`) — used when `per_pipe` is absent.
/// 3. **defaults** (10 attempts, 10s base, 5m cap
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
