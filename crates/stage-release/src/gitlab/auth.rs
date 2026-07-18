use super::*;

/// Build the GitLab auth header name and value for the given token.
pub(crate) fn auth_header(use_job_token: bool) -> &'static str {
    if use_job_token {
        "JOB-TOKEN"
    } else {
        "PRIVATE-TOKEN"
    }
}

/// Resolve whether the `JOB-TOKEN` header should be used for the given token.
///
/// Decide whether to send a JOB-TOKEN header.
/// Returns true only when all three hold:
///
/// 1. `CI_JOB_TOKEN` env var is non-empty (we're inside a GitLab runner).
/// 2. `gitlab_urls.use_job_token` is true in config.
/// 3. the token being used equals `CI_JOB_TOKEN` — so secondary clients built
///    during the same CI run (e.g. Homebrew publishing with a personal token)
///    still fall back to `PRIVATE-TOKEN`.
///
/// Production wires up [`ProcessEnvSource`] via
/// [`anodizer_core::Context::env_source`]; tests inject a
/// [`anodizer_core::MapEnvSource`] so the `CI_JOB_TOKEN` branches can
/// be driven without mutating the process env.
pub(crate) fn resolve_use_job_token_with_env<E: EnvSource + ?Sized>(
    config_flag: bool,
    token: &str,
    env: &E,
) -> bool {
    let ci_token = env.var("CI_JOB_TOKEN").unwrap_or_default();
    if ci_token.is_empty() {
        return false;
    }
    if !config_flag {
        return false;
    }
    token == ci_token
}
