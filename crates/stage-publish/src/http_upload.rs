//! Shared helpers for HTTP-based publishers (Artifactory + generic uploads).
//!
//! Both publishers walk the same per-entry credential cascade
//! (config → env), the same mTLS pair-check, and the same anonymous-upload
//! refusal pattern. The patterns used to be open-coded twice; this module
//! is the single source of truth so a fix in one place reaches both.

use anodizer_core::context::Context;
use anyhow::{Context as _, Result, bail};

/// Caller spec for `resolve_http_credentials`. Captures the small handful
/// of per-publisher knobs that distinguish artifactory's refusal of
/// anonymous uploads from upload's tolerance for them, plus the env-var
/// prefix used to look up secrets.
pub(crate) struct CredentialResolveSpec<'a> {
    /// Publisher label used in error messages ("artifactory" / "upload").
    pub publisher: &'a str,
    /// Entry name; appears in error messages and joined into env-var keys.
    pub entry_name: &'a str,
    /// Optional `username:` value from the publisher entry config.
    pub config_username: Option<&'a str>,
    /// Optional `password:` value from the publisher entry config.
    pub config_password: Option<&'a str>,
    /// Env-var prefix (e.g. "ARTIFACTORY", "UPLOAD"). Joined with the
    /// upper-cased entry name and `_USERNAME` / `_SECRET`.
    pub env_prefix: &'a str,
    /// When false (artifactory), an unresolved credential pair bails. When
    /// true (upload), an entirely empty pair is acceptable for anonymous
    /// targets and we only refuse the half-set state.
    pub anonymous_ok: bool,
}

/// Resolve `(username, password)` for an HTTP-based publisher entry.
///
/// Cascade per field: rendered config value → `<PREFIX>_<NAME>_USERNAME` /
/// `<PREFIX>_<NAME>_SECRET` env var. Empty-after-render falls through to
/// env so a half-edited YAML does not silently ship anonymous.
///
/// Refuses (with a clear "set X or env Y" message):
/// - Half-set credential pair under any spec.
/// - Empty pair when `anonymous_ok = false` (artifactory).
///
/// Skipped in dry-run so config previews do not require real secrets.
pub(crate) fn resolve_http_credentials(
    ctx: &Context,
    spec: &CredentialResolveSpec<'_>,
) -> Result<(String, String)> {
    let env_map = ctx.template_vars().all_env();
    let lookup_env = |name: &str| -> Option<String> {
        env_map
            .get(name)
            .cloned()
            .or_else(|| ctx.env_var(name))
            .filter(|s| !s.is_empty())
    };

    let name_upper = spec.entry_name.to_uppercase().replace('-', "_");
    let username_env = format!("{}_{}_USERNAME", spec.env_prefix, name_upper);
    let password_env = format!("{}_{}_SECRET", spec.env_prefix, name_upper);

    // Username: render config (if set), fall through on empty render.
    let username = match spec.config_username {
        Some(u) => {
            let rendered = ctx.render_template(u).with_context(|| {
                format!(
                    "{}: failed to render username for '{}'",
                    spec.publisher, spec.entry_name
                )
            })?;
            if rendered.is_empty() {
                lookup_env(&username_env).unwrap_or_default()
            } else {
                rendered
            }
        }
        None => lookup_env(&username_env).unwrap_or_default(),
    };

    // Password: config wins over env so a YAML setting isn't shadowed by
    // a stale shell env var.
    let password = spec
        .config_password
        .and_then(|p| ctx.render_template(p).ok())
        .filter(|p| !p.is_empty())
        .or_else(|| lookup_env(&password_env))
        .unwrap_or_default();

    if !ctx.is_dry_run() {
        match (username.is_empty(), password.is_empty()) {
            (false, true) => bail!(
                "{}: '{}' has username set but no password \
                 (set 'password:' in config or {} in env)",
                spec.publisher,
                spec.entry_name,
                password_env
            ),
            (true, false) => bail!(
                "{}: '{}' has password set but no username \
                 (set 'username:' in config or {} in env)",
                spec.publisher,
                spec.entry_name,
                username_env
            ),
            (true, true) if !spec.anonymous_ok => bail!(
                "{}: '{}' resolved with no credentials \
                 (set username/password in config or {} / {} in env; \
                 anonymous upload is refused)",
                spec.publisher,
                spec.entry_name,
                username_env,
                password_env
            ),
            _ => {}
        }
    }

    Ok((username, password))
}

/// Refuse a half-set mTLS pair. Both crates need the same exact check.
pub(crate) fn validate_mtls_pair(
    publisher: &str,
    entry_name: &str,
    cert: Option<&str>,
    key: Option<&str>,
) -> Result<()> {
    if cert.is_some() != key.is_some() {
        bail!(
            "{}: '{}' has only one of client_x509_cert / client_x509_key set \
             (set both to enable mTLS, or leave both empty)",
            publisher,
            entry_name
        );
    }
    Ok(())
}
