use anodizer_core::context::Context;

/// Skip reason recorded when a keyless cosign sign config is bypassed under
/// the determinism harness.
pub(crate) const KEYLESS_COSIGN_HARNESS_SKIP: &str = "keyless cosign cannot sign in the determinism harness (no ambient OIDC); \
     signatures are non-deterministic and allowlisted";

/// True when a sign config invokes keyless cosign: resolved `cmd` basename is
/// exactly `cosign` and no arg supplies `--key`. Keyless mode is the path
/// that talks to Fulcio and lazily initializes the `~/.sigstore` TUF trust
/// root on a fresh host.
pub(crate) fn is_keyless_cosign(cmd: &str, args: &[String]) -> bool {
    // Compare the basename so an absolute/relative path to cosign still matches.
    let basename = std::path::Path::new(cmd)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(cmd);
    if basename != "cosign" {
        return false;
    }
    // A `--key` (the keyed form, e.g. `--key=env://COSIGN_KEY`) signs with a
    // local key and never contacts Fulcio. The flag is a literal, so the raw
    // (unrendered) args are sufficient to detect it.
    let has_key = args.iter().any(|a| a == "--key" || a.starts_with("--key="));
    !has_key
}

/// True when a sign config is keyless cosign AND the determinism harness is
/// active.
///
/// Shared by the `signs` / `binary_signs` loop here and the `docker_signs`
/// loop in `lib.rs`. The discriminator is purely `cmd == cosign` + absence of
/// `--key`, so it is config-mode-agnostic (single-crate, workspace-lockstep,
/// workspace per-crate all flow through these loops). The harness signal
/// mirrors the `IsHarness` derivation in `Context::populate_runtime_vars`:
/// the `ANODIZER_IN_DETERMINISM_HARNESS` env var is set.
pub(crate) fn is_keyless_cosign_under_harness(cmd: &str, args: &[String], ctx: &Context) -> bool {
    if ctx.env_var("ANODIZER_IN_DETERMINISM_HARNESS").is_none() {
        return false;
    }
    is_keyless_cosign(cmd, args)
}

/// Force keyed cosign signing fully offline under the determinism harness by
/// appending `--tlog-upload=false` to its args.
///
/// By default `cosign sign` / `sign-blob` upload the signature to the public
/// Rekor transparency log, which makes cosign fetch its signing config from
/// sigstore's TUF CDN over the network. That network dependency violates the
/// harness's hermeticity contract: a flaked DNS lookup on a CI runner fails an
/// otherwise byte-reproducible rebuild. The harness signs with throwaway
/// ephemeral keys purely to exercise the sign stage; the real
/// `release --publish-only` step re-signs with the production key on a
/// networked runner and keeps tlog transparency, so suppressing the upload
/// here loses nothing real while guaranteeing the harness never touches the
/// network for any consumer's cosign config.
///
/// A no-op (returns `args` unchanged) unless ALL hold: the harness is active,
/// `cmd`'s basename is `cosign`, some arg supplies `--key` (keyless cosign is
/// skipped upstream and the flag is meaningless without a key), and no arg
/// already pins `--tlog-upload` (an explicit operator choice is respected,
/// making this idempotent). cosign accepts the flag interspersed with or after
/// positionals, so appending is always safe.
pub(crate) fn harden_cosign_args_for_harness(
    cmd: &str,
    mut args: Vec<String>,
    ctx: &Context,
) -> Vec<String> {
    if ctx.env_var("ANODIZER_IN_DETERMINISM_HARNESS").is_none() {
        return args;
    }
    let basename = std::path::Path::new(cmd)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(cmd);
    if basename != "cosign" {
        return args;
    }
    let has_key = args.iter().any(|a| a == "--key" || a.starts_with("--key="));
    if !has_key {
        return args;
    }
    let already_pinned = args
        .iter()
        .any(|a| a == "--tlog-upload" || a.starts_with("--tlog-upload="));
    if already_pinned {
        return args;
    }
    args.push("--tlog-upload=false".to_string());
    args
}

/// True when `cmd`'s basename identifies the cosign binary (matches `cosign`
/// and `cosign-*` variants).
///
/// Single source for the cosign-basename test shared by the consent-side
/// (`ensure_cosign_consent_env`) and the signing-requirement derivation
/// (`entry_env_requirements`'s `KeyEnv{Cosign}` site) so the two cannot drift.
pub(crate) fn is_cosign_cmd(cmd: &str) -> bool {
    std::path::Path::new(cmd)
        .file_name()
        .and_then(|b| b.to_str())
        .is_some_and(|b| b.starts_with("cosign"))
}

/// Env var name carrying cosign's non-interactive consent (the argv equivalent
/// is the global `--yes`/`-y` flag).
pub(crate) const COSIGN_CONSENT_ENV: &str = "COSIGN_YES";

/// Ensure a cosign invocation runs non-interactively by exporting
/// `COSIGN_YES=true` in its child env.
///
/// Without consent, `cosign sign` / `sign-blob` print the sigstore privacy
/// banner ("Note that there may be personally identifiable information … By
/// typing 'y' you attest …") and block on a `y/N` prompt — there is no TTY in
/// CI, so the prompt hangs or the banner pollutes the log. cosign's documented
/// non-interactive consent is the global `--yes` flag or its `COSIGN_YES` env
/// equivalent; the env form is preferred here because it is subcommand- and
/// arg-position-agnostic (one seam covers `sign`, `sign-blob`, and any
/// user-supplied args) and cannot collide with a positional the user wrote.
///
/// A no-op for non-cosign signers. Idempotent and operator-respecting: an
/// explicit `COSIGN_YES` already present in the rendered env (e.g. a user who
/// set it to `false` to force interactivity) is left untouched.
pub(crate) fn ensure_cosign_consent_env(cmd: &str, env: &mut Vec<(String, String)>) {
    if !is_cosign_cmd(cmd) {
        return;
    }
    if env.iter().any(|(k, _)| k == COSIGN_CONSENT_ENV) {
        return;
    }
    env.push((COSIGN_CONSENT_ENV.to_string(), "true".to_string()));
}
