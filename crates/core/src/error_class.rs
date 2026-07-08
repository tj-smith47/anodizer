//! Deterministic-error classification for the CLI exit contract.
//!
//! Retry wrappers around anodizer (CI actions, shell loops) need to tell a
//! transient failure (network flake, registry 5xx — worth retrying) from a
//! deterministic one (unparseable config, invalid flag values, the
//! dist-not-empty guard — retrying burns attempts on an identical failure).
//! Two signals carry that classification:
//!
//! * **exit code [`EXIT_DETERMINISTIC`] (2)** — the Unix usage-error
//!   convention; everything else keeps exit 1.
//! * **stderr marker [`CLASS_MARKER`]** — a machine-readable line for
//!   wrappers that cannot rely on the exit code (or pin an older anodizer
//!   whose deterministic paths still exited 1).
//!
//! Classification is a conservative allowlist: an error is deterministic
//! only if its construction site wrapped it via [`deterministic`] /
//! [`deterministic_msg`]. Anything unwrapped — however config-shaped its
//! message looks — stays exit 1, so a transient failure can never be
//! misfiled as unretryable.

use std::fmt;

/// Exit code for deterministic config/usage errors (Unix usage-error
/// convention, and the code clap already uses for flag-parse errors).
pub const EXIT_DETERMINISTIC: i32 = 2;

/// Machine-readable stderr marker emitted alongside every deterministic
/// error, mirroring the `anodizer-output <key>=<value>` payload convention.
pub const CLASS_MARKER: &str = "anodizer-error-class: deterministic";

/// Transparent marker wrapper: its presence anywhere in an `anyhow` chain
/// classifies the whole error as deterministic. Display and `source()`
/// forward to the wrapped error, so rendered output (top message + `caused
/// by:` chain) is byte-identical to the unwrapped error.
#[derive(Debug)]
pub struct DeterministicError(anyhow::Error);

impl fmt::Display for DeterministicError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for DeterministicError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // Deref to the wrapped error's top and continue ITS chain — the
        // wrapper's own Display already carries the top message, so
        // returning the top itself would print it twice.
        self.0.source()
    }
}

/// Mark an error as deterministic. The returned error renders identically;
/// only [`is_deterministic`] can tell the difference.
pub fn deterministic(err: anyhow::Error) -> anyhow::Error {
    anyhow::Error::new(DeterministicError(err))
}

/// [`deterministic`] for message-shaped errors — the `Result<_, String>`
/// validator idiom (`.map_err(deterministic_msg)?`).
pub fn deterministic_msg<M>(msg: M) -> anyhow::Error
where
    M: fmt::Display + fmt::Debug + Send + Sync + 'static,
{
    deterministic(anyhow::Error::msg(msg))
}

/// True when any link of the chain is a [`DeterministicError`] marker —
/// context layered on top of a marked error does not hide it.
pub fn is_deterministic(err: &anyhow::Error) -> bool {
    err.chain().any(|c| c.is::<DeterministicError>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context as _;

    #[test]
    fn plain_error_is_not_deterministic() {
        let err = anyhow::anyhow!("network timeout");
        assert!(!is_deterministic(&err));
    }

    #[test]
    fn wrapped_error_is_deterministic() {
        let err = deterministic(anyhow::anyhow!("bad config"));
        assert!(is_deterministic(&err));
    }

    #[test]
    fn msg_wrapper_is_deterministic() {
        let err = deterministic_msg("unknown publisher 'nmp'".to_string());
        assert!(is_deterministic(&err));
    }

    #[test]
    fn context_on_top_still_detected() {
        let err: anyhow::Error = Err::<(), _>(deterministic_msg("parse failure"))
            .context("failed to load config")
            .unwrap_err();
        assert!(is_deterministic(&err));
    }

    #[test]
    fn display_and_chain_render_identically_to_unwrapped() {
        let unwrapped: anyhow::Error = Err::<(), _>(anyhow::anyhow!("root cause"))
            .context("middle")
            .context("top")
            .unwrap_err();
        let wrapped = deterministic(
            Err::<(), _>(anyhow::anyhow!("root cause"))
                .context("middle")
                .context("top")
                .unwrap_err(),
        );
        assert_eq!(wrapped.to_string(), unwrapped.to_string());
        let unwrapped_chain: Vec<String> = unwrapped.chain().map(|c| c.to_string()).collect();
        let wrapped_chain: Vec<String> = wrapped.chain().map(|c| c.to_string()).collect();
        assert_eq!(wrapped_chain, unwrapped_chain);
    }
}
