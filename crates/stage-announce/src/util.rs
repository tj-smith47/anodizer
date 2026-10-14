//! Shared validation helpers for announce providers.
//!
//! Lifted out of per-provider files to remove the duplicated min-length
//! token shape check. Each provider keeps its own provider-specific
//! `validate_token_shape` function on top of this shared base check
//! (e.g. LinkedIn JWT segment count, OpenCollective whitespace check).

use anyhow::Result;

/// Reject obvious non-credential values for a provider's API token: a
/// missing-credential mistake (empty / placeholder) usually shows up as
/// a too-short string, and the upstream API responds with an opaque 401
/// that's hard to debug. Catching it here reports the misconfiguration
/// at validation time with a clear message.
///
/// `provider` is a short label (e.g. "linkedin"); `env_var` names the env
/// variable the user set (e.g. "LINKEDIN_ACCESS_TOKEN"); `min_len` is the
/// per-provider lower bound.
pub fn validate_token_min_length(
    provider: &str,
    env_var: &str,
    token: &str,
    min_len: usize,
) -> Result<()> {
    if token.len() < min_len {
        anyhow::bail!(
            "announce.{provider}: {env_var} looks too short ({} chars, expected at least {min_len})",
            token.len()
        );
    }
    Ok(())
}
