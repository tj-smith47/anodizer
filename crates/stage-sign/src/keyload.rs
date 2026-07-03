//! Offline cosign signing-key load verification for the preflight gate.
//!
//! `cosign public-key --key <ref>` reads the private key referenced by
//! `<ref>`, decrypts it (consulting `COSIGN_PASSWORD` for an encrypted key),
//! and prints the derived public key. It performs no transparency-log upload
//! and contacts no network — it is a purely local key-load round-trip. The
//! preflight gate runs it before a tag is cut so a key whose password is wrong
//! or missing fails the gate up front instead of wasting an entire
//! build + determinism run that would only fail later in the sign stage.
//!
//! The check is adaptive across key encryption: an UNENCRYPTED key loads with
//! an empty `COSIGN_PASSWORD`; an ENCRYPTED key requires the correct password.
//! Both are validated by the same load — no static "password must be non-empty"
//! rule (which would false-fail an unencrypted production key).

use anodizer_core::{EnvSource, ProcessEnvSource};
use std::process::Command;

/// Cosign's own env var for an encrypted private key's password.
const COSIGN_PASSWORD_ENV: &str = "COSIGN_PASSWORD";

/// Outcome of an offline cosign key-load verification.
#[derive(Debug)]
pub enum CosignKeyLoad {
    /// The key reference loaded successfully (the password decrypted it, or it
    /// was unencrypted and loaded with an empty password).
    Loaded,
    /// `cosign` is not installed on this runner, so the load could not be
    /// attempted. The caller should WARN (not fail): the key/password combo is
    /// validated at sign time on a runner that does carry cosign.
    CosignUnavailable,
    /// The probe for `cosign` itself errored with a non-`NotFound` I/O failure
    /// (e.g. permission denied spawning the version check), so its presence
    /// could NOT be determined — distinct from a definitive "absent" (a
    /// not-on-PATH `NotFound` folds into [`CosignKeyLoad::CosignUnavailable`]).
    /// Carries the probe error so the caller can name it
    /// instead of masquerading the failure as a clean absence. Like
    /// [`CosignKeyLoad::CosignUnavailable`] the caller WARNs (sign time
    /// re-validates), but the surfaced reason tells the operator the precheck
    /// was skipped because the probe broke, not because cosign is missing.
    CosignProbeFailed(String),
    /// `cosign` is installed but the key failed to load — a genuinely bad
    /// secret (wrong or missing password for an encrypted key, malformed key
    /// material). Carries cosign's diagnostic for the operator. The caller
    /// must treat this as a hard preflight failure.
    Failed(String),
}

/// Offline-verify that the cosign private key referenced by `key_ref` (e.g.
/// `env://COSIGN_KEY`) loads with the resolved `COSIGN_PASSWORD`.
///
/// Delegates to [`verify_cosign_key_loads_with_env`] with a
/// [`ProcessEnvSource`], so the relevant secrets are resolved from the real
/// process environment (the same secrets the preflight job already injects)
/// and forwarded explicitly onto the spawned `cosign` command.
pub fn verify_cosign_key_loads(key_ref: &str) -> CosignKeyLoad {
    verify_cosign_key_loads_with_env(key_ref, &ProcessEnvSource)
}

/// [`EnvSource`]-injecting form of [`verify_cosign_key_loads`].
///
/// Spawns `cosign public-key --key <key_ref>`, which reads and decrypts the
/// private key locally and prints its public half. The secrets cosign needs
/// are resolved from `env` and forwarded **explicitly** onto the child
/// command (never relied on as ambient inheritance, so the path is testable
/// without mutating the process env):
///
/// - If `key_ref` is an `env://VAR` reference and `VAR` is present in `env`,
///   that `VAR=<value>` is set on the command so cosign can resolve the key.
/// - If `COSIGN_PASSWORD` is present in `env`, it is set on the command so an
///   encrypted key decrypts.
///
/// A secret absent from `env` is left unset on the command, so the child's
/// default ambient inheritance still applies (production behavior is
/// byte-identical to consulting the inherited environment). `COSIGN_YES` is
/// exported so the run is non-interactive. No tlog, no network, no publish.
///
/// Returns [`CosignKeyLoad::CosignUnavailable`] when cosign is absent (caller
/// WARNs), [`CosignKeyLoad::CosignProbeFailed`] when the availability probe
/// itself errored (caller WARNs, naming the probe error), [`CosignKeyLoad::Loaded`]
/// on a successful load, and [`CosignKeyLoad::Failed`] with cosign's stderr when
/// the key fails to load.
pub fn verify_cosign_key_loads_with_env(key_ref: &str, env: &dyn EnvSource) -> CosignKeyLoad {
    match anodizer_core::tool_detect::runs("cosign") {
        anodizer_core::tool_detect::ToolProbe::Available => {}
        // Definitively absent: the load can't be attempted; sign time
        // re-validates on a runner that carries cosign.
        anodizer_core::tool_detect::ToolProbe::Unavailable => {
            return CosignKeyLoad::CosignUnavailable;
        }
        // Probe failure (e.g. permission denied) means presence is UNKNOWN,
        // not "absent": surface the I/O error so the caller's WARN names it
        // rather than masquerading a broken probe as a clean skip of this
        // security-relevant precheck.
        anodizer_core::tool_detect::ToolProbe::ProbeFailed(e) => {
            return CosignKeyLoad::CosignProbeFailed(format!(
                "could not probe cosign availability: {e}"
            ));
        }
    }

    // `--key=<ref>` (single-token form) so a `key_ref` that itself contains a
    // shell-significant char cannot be misparsed as a separate positional.
    let mut command = Command::new("cosign");
    command
        .arg("public-key")
        .arg(format!("--key={key_ref}"))
        .current_dir(anodizer_core::path_util::probe_dir())
        .env(crate::process::COSIGN_CONSENT_ENV, "true");

    // Forward the key var explicitly when `key_ref` is `env://VAR`: cosign
    // resolves the ref by reading `VAR` from its OWN child environment, so the
    // value must be planted on the command rather than left to ambient inherit.
    if let Some(var) = key_ref.strip_prefix("env://") {
        if let Some(value) = env.var(var) {
            command.env(var, value);
        }
    }
    // Forward COSIGN_PASSWORD explicitly so an encrypted key can decrypt.
    if let Some(password) = env.var(COSIGN_PASSWORD_ENV) {
        command.env(COSIGN_PASSWORD_ENV, password);
    }

    let output = match command.output() {
        Ok(o) => o,
        Err(e) => {
            return CosignKeyLoad::Failed(format!(
                "spawning `cosign public-key --key={key_ref}` failed: {e}"
            ));
        }
    };
    if output.status.success() {
        return CosignKeyLoad::Loaded;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    let detail = if detail.is_empty() {
        format!("cosign exited {}", output.status)
    } else {
        detail.to_string()
    };
    CosignKeyLoad::Failed(detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::{MapEnvSource, harness_signing};

    /// Probe cosign for a gated test: `true` when present. A probe ERROR is
    /// surfaced through `reason` (never silently collapsed into a bare
    /// "absent"), so a skipped test records why cosign was unusable.
    fn cosign_present() -> (bool, String) {
        use anodizer_core::tool_detect::ToolProbe;
        match anodizer_core::tool_detect::runs("cosign") {
            ToolProbe::Available => (true, "cosign=present".to_string()),
            ToolProbe::Unavailable => (false, "cosign=absent".to_string()),
            ToolProbe::ProbeFailed(e) => (false, format!("cosign=probe-error({e})")),
        }
    }

    /// Generates an ephemeral ENCRYPTED cosign keypair (the harness uses a
    /// non-empty password), then asserts the load is adaptive on the password:
    /// the correct password LOADS the key (`Loaded`); a wrong password FAILS
    /// (`Failed`). Skips cleanly when cosign is absent so CI without cosign
    /// stays green.
    #[test]
    fn correct_password_loads_wrong_password_fails() {
        let (present, reason) = cosign_present();
        if !present {
            eprintln!("skipping correct_password_loads_wrong_password_fails: {reason}");
            return;
        }
        // Provisioning writes the ephemeral cosign.key into a tempdir and
        // returns its PEM contents + the password that encrypts it.
        let keys = harness_signing::provision_ephemeral_keys(1_715_000_000)
            .expect("provision ephemeral cosign keypair");

        // env://COSIGN_KEY load with the CORRECT password must succeed. Secrets
        // are injected through the EnvSource seam, not the process env, so the
        // test never races a parallel test over the global env.
        let good_env = MapEnvSource::new()
            .with("COSIGN_KEY", &keys.cosign_key_contents)
            .with("COSIGN_PASSWORD", &keys.cosign_password);
        let ok = verify_cosign_key_loads_with_env("env://COSIGN_KEY", &good_env);
        assert!(
            matches!(ok, CosignKeyLoad::Loaded),
            "correct password must load the key, got {ok:?}"
        );

        // A WRONG password must fail the load (the harness key is encrypted).
        let bad_env = MapEnvSource::new()
            .with("COSIGN_KEY", &keys.cosign_key_contents)
            .with("COSIGN_PASSWORD", "definitely-not-the-password");
        let bad = verify_cosign_key_loads_with_env("env://COSIGN_KEY", &bad_env);
        assert!(
            matches!(bad, CosignKeyLoad::Failed(_)),
            "wrong password must fail to load the encrypted key, got {bad:?}"
        );
    }

    /// False-positive guard: an UNENCRYPTED cosign key (generated with an empty
    /// `COSIGN_PASSWORD`) must LOAD with an empty password. This proves the
    /// adaptive load imposes no "password must be non-empty" rule — such a rule
    /// would false-fail an unencrypted production key at preflight. Generates
    /// the key in a tempdir (cosign's default `cosign.key`/`cosign.pub` names,
    /// no `--output-key-prefix` for older-cosign compatibility) and points
    /// `env://COSIGN_KEY` at its PEM. Skips cleanly when cosign is absent.
    #[test]
    fn unencrypted_key_loads_with_empty_password() {
        use std::process::Command;
        let (present, reason) = cosign_present();
        if !present {
            eprintln!("skipping unencrypted_key_loads_with_empty_password: {reason}");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir for unencrypted keygen");
        // Empty COSIGN_PASSWORD ⇒ cosign writes an UNENCRYPTED private key.
        let out = Command::new("cosign")
            .args(["generate-key-pair"])
            .current_dir(tmp.path())
            .env("COSIGN_PASSWORD", "")
            .output()
            .expect("spawn cosign generate-key-pair");
        assert!(
            out.status.success(),
            "cosign generate-key-pair (unencrypted) must succeed: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let pem = std::fs::read_to_string(tmp.path().join("cosign.key"))
            .expect("read generated unencrypted cosign.key");

        // Secrets injected through the EnvSource seam (not the process env): an
        // empty COSIGN_PASSWORD is forwarded explicitly so cosign loads the
        // unencrypted key without an interactive prompt.
        let env = MapEnvSource::new()
            .with("COSIGN_KEY", &pem)
            .with("COSIGN_PASSWORD", "");
        let loaded = verify_cosign_key_loads_with_env("env://COSIGN_KEY", &env);
        assert!(
            matches!(loaded, CosignKeyLoad::Loaded),
            "an unencrypted key must load with an empty password (no non-empty-password rule), got {loaded:?}"
        );
    }
}
