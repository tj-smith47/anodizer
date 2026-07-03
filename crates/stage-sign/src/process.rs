//! Shared sign processing — the core driver behind both `signs:` (normal
//! artifact signing) and `binary_signs:` (per-binary signing). Owns the
//! `SignJob` value type, the `process_sign_configs` driver, and the
//! parallel-execution wrapper.
//!
//! Split out from `lib.rs` so the per-job flow (filter → render → execute)
//! is independently reviewable without scrolling through SignStage glue.

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result};

use anodizer_core::EnvSource;
use anodizer_core::artifact::ArtifactKind;
use anodizer_core::config::SignConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::target::map_target;

use crate::helpers::{
    build_authenticode_argv, default_sign_cmd, expand_shell_vars, prepare_stdin_from,
    redact_password_in_argv, resolve_sign_args, resolve_signature_path, should_sign_artifact,
    windows_artifact_extension_matches,
};

/// Skip reason recorded when a keyless cosign sign config is bypassed under
/// the determinism harness.
pub(crate) const KEYLESS_COSIGN_HARNESS_SKIP: &str = "keyless cosign cannot sign in the determinism harness (no ambient OIDC); \
     signatures are non-deterministic and allowlisted";

/// True when a sign config is keyless cosign (resolved `cmd` basename is
/// `cosign`, no explicit `--key` arg) AND the determinism harness is active.
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
    // Compare the basename so an absolute/relative path to cosign still matches.
    let basename = std::path::Path::new(cmd)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(cmd);
    if basename != "cosign" {
        return false;
    }
    // A `--key` (the keyed form, e.g. `--key=env://COSIGN_KEY`) signs with the
    // harness's ephemeral key and must still run. The flag is a literal, so the
    // raw (unrendered) args are sufficient to detect it.
    let has_key = args.iter().any(|a| a == "--key" || a.starts_with("--key="));
    !has_key
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

/// Artifact filter mode for `process_sign_configs`.
#[derive(Clone, Copy)]
pub(crate) enum ArtifactFilter {
    /// Use the `artifacts` field from each SignConfig (or default to "none").
    FromConfig,
    /// Always restrict to `ArtifactKind::Binary`, regardless of config.
    BinaryOnly,
}

/// Append a target triple to a basename while keeping its extension
/// suffix: `anodizer.sig` + `aarch64-apple-darwin` →
/// `anodizer-aarch64-apple-darwin.sig`, `anodizer.exe.sig` →
/// `anodizer.exe-aarch64-pc-windows-msvc.sig`. A basename with no
/// extension gets a plain `-<target>` suffix.
fn qualify_basename_with_target(name: &str, target: &str) -> String {
    let path = std::path::Path::new(name);
    match (
        path.file_stem().and_then(|s| s.to_str()),
        path.extension().and_then(|e| e.to_str()),
    ) {
        (Some(stem), Some(ext)) => format!("{stem}-{target}.{ext}"),
        _ => format!("{name}-{target}"),
    }
}

/// A fully-prepared sign job ready for parallel execution.
///
/// All template rendering and path resolution is done up-front so that the
/// actual signing command can be spawned without borrowing the `Context`.
struct SignJob {
    /// The signing command binary (e.g., "gpg", "cosign").
    cmd: String,
    /// Fully-resolved command arguments.
    args: Vec<String>,
    /// Optional stdin content to pipe to the signing command.
    stdin_data: Option<Vec<u8>>,
    /// Optional environment variables to set on the child process, ordered.
    env: Option<Vec<(String, String)>>,
    /// Extra secret values to scrub from the child's stdout/stderr regardless
    /// of whether they are exported as child env. Each entry is a
    /// `(synthetic_key, value)` pair fed to [`anodizer_core::redact::string`];
    /// the key only governs the masked replacement spelling, so it is chosen to
    /// always trip `is_secret` (e.g. a `*_PASSWORD` suffix). The Authenticode
    /// path uses this for the cert password — which is passed in argv, never
    /// deliberately exported to the child env — so a tool echoing it on error
    /// is still masked even when the user's `password_env` key carries no
    /// secret suffix.
    redact_extra: Vec<(String, String)>,
    /// Env var names to strip from the child's *inherited* environment before
    /// spawning. `Command` does not `env_clear`, so the child would otherwise
    /// inherit the whole parent env. The Authenticode path lists its
    /// `password_env` here so the cert password reaches the signer only via
    /// argv (`-pass`/`/p`) and never as an inherited env var a misbehaving
    /// tool could dump. Empty for every other job.
    env_remove: Vec<String>,
    /// Human-readable label for log messages (e.g., "sign", "binary-sign").
    label: String,
    /// The sign config's `id` field for log messages.
    id_label: String,
    /// Display string for the artifact being signed (used in log messages).
    artifact_display: String,
    /// Display string for the signature output path (used in log messages).
    signature_display: String,
    /// Whether to capture and log the command's stdout/stderr.
    output_flag: bool,
    /// Artifact registrations to add after signing (signature + optional certificate).
    new_artifacts: Vec<anodizer_core::artifact::Artifact>,
    /// `(from, to)` atomic rename applied after the signer exits 0.
    ///
    /// osslsigncode requires a distinct `-out` path, so the Authenticode job
    /// signs to a sibling temp (`from`) and then renames it over the original
    /// artifact (`to`). `None` for every detached (cosign/gpg) job and for the
    /// in-place signtool path.
    rename_after: Option<(std::path::PathBuf, std::path::PathBuf)>,
    /// When set, the per-artifact RESULT line emitted at status level after a
    /// successful Authenticode sign (e.g. `authenticode-signed myapp.exe`). The
    /// detached path leaves this `None` (its result is the registered `.sig`).
    authenticode_result: Option<String>,
}

/// Best-effort removal of an Authenticode job's `-out` temp on the error path.
///
/// The osslsigncode path signs to a sibling temp (`rename_after.0`) and only
/// renames it over the original on success. Any failure before the rename
/// (spawn, wait, or a non-zero signer exit) must clean up the partial temp so
/// no `.authenticode-tmp` litter file is left behind. No-op for the detached
/// (cosign/gpg) and in-place signtool paths, which carry no `rename_after`.
fn cleanup_rename_temp(job: &SignJob) {
    if let Some((from, _)) = &job.rename_after {
        let _ = std::fs::remove_file(from);
    }
}

/// Execute a single prepared sign job, returning `Ok(())` on success.
fn execute_sign_job(job: &SignJob, log: &StageLogger) -> Result<()> {
    // Per-artifact detail — at default verbosity the `signing N artifacts`
    // summary (emitted once before this loop) is the status-level signal; the
    // per-artifact `sign X → Y` line would flood the log on wide fan-outs.
    log.verbose(&format!(
        "signing {} → {} ({}[{}])",
        job.artifact_display, job.signature_display, job.label, job.id_label
    ));

    let stdin_cfg = if job.stdin_data.is_some() {
        Stdio::piped()
    } else {
        Stdio::inherit()
    };

    let mut command = Command::new(&job.cmd);
    command
        .args(&job.args)
        .stdin(stdin_cfg)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(ref env_vars) = job.env {
        for (k, v) in env_vars {
            command.env(k, v);
        }
    }
    // Strip inherited secret env vars (e.g. the Authenticode `password_env`) so
    // the secret reaches the signer only via argv, not as a child env var.
    for k in &job.env_remove {
        command.env_remove(k);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            cleanup_rename_temp(job);
            return Err(e).with_context(|| {
                format!(
                    "{}: failed to spawn '{}' for {}",
                    job.label, job.cmd, job.artifact_display
                )
            });
        }
    };

    if let Some(ref data) = job.stdin_data {
        if let Some(mut child_stdin) = child.stdin.take() {
            child_stdin.write_all(data).with_context(|| {
                format!(
                    "{}: failed to write stdin for {}",
                    job.label, job.artifact_display
                )
            })?;
            drop(child_stdin); // Explicitly close stdin so child sees EOF
        } else {
            // Proceeding would run the signer WITHOUT its intended stdin,
            // producing a signature over missing input. Fail hard instead.
            cleanup_rename_temp(job);
            anyhow::bail!(
                "{}: stdin data was provided but the child process stdin is \
                 unavailable for {} — refusing to sign without it",
                job.label,
                job.artifact_display
            );
        }
    }

    let output = match child.wait_with_output() {
        Ok(output) => output,
        Err(e) => {
            cleanup_rename_temp(job);
            return Err(e).with_context(|| {
                format!(
                    "{}: failed to wait for '{}' for {}",
                    job.label, job.cmd, job.artifact_display
                )
            });
        }
    };

    // Redact secrets from stdout/stderr before any output or logging.
    // The scrub set is the child env PLUS `redact_extra` (secrets passed via
    // argv, e.g. the Authenticode cert password, which the child env never
    // carries) PLUS the process environment. `redact::string` masks each entry
    // whose key trips `is_secret`; `redact_extra` keys are chosen to always
    // trip it, so the value is masked regardless of the user's env-var name.
    let env_pairs: Vec<(String, String)> = job
        .env
        .iter()
        .flat_map(|m| m.iter().cloned())
        .chain(job.redact_extra.iter().cloned())
        .chain(std::env::vars())
        .collect();

    let stdout_raw = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr_raw = String::from_utf8_lossy(&output.stderr).to_string();

    let stdout_str = anodizer_core::redact::string(&stdout_raw, &env_pairs);
    let stderr_str = anodizer_core::redact::string(&stderr_raw, &env_pairs);

    // Raw subprocess stdio is verbose-only detail per
    // .claude/rules/log-status-vs-verbose.md; an explicit `output:` opts the
    // tee back in but it stays below default. A non-zero exit still surfaces
    // via `check_output` below.
    if job.output_flag {
        if !stdout_str.is_empty() {
            log.verbose(&format!("[{} stdout] {}", job.label, stdout_str.trim()));
        }
        if !stderr_str.is_empty() {
            log.verbose(&format!("[{} stderr] {}", job.label, stderr_str.trim()));
        }
    }

    let mut redacted_output = output;
    redacted_output.stdout = stdout_str.into_bytes();
    redacted_output.stderr = stderr_str.into_bytes();

    if let Err(e) = log.check_output(redacted_output, &job.cmd) {
        // A non-zero signer exit may have left a partial `-out` temp; remove it
        // so a failed Authenticode sign leaves neither a clobbered original nor
        // a `.authenticode-tmp` litter file behind.
        cleanup_rename_temp(job);
        return Err(e);
    }

    // Authenticode (osslsigncode) writes to a sibling temp; atomically replace
    // the original artifact only after the signer succeeded so a failed sign
    // never leaves a half-written file in place.
    if let Some((from, to)) = &job.rename_after {
        if let Err(e) = std::fs::rename(from, to) {
            // A failed rename (e.g. cross-device, permissions) would otherwise
            // strand the signed temp next to the untouched original; remove it
            // so the error path leaves no `.authenticode-tmp` litter behind.
            cleanup_rename_temp(job);
            return Err(e).with_context(|| {
                format!(
                    "{}: failed to move signed temp {} over {}",
                    job.label,
                    from.display(),
                    to.display()
                )
            });
        }
    }

    if let Some(result) = &job.authenticode_result {
        log.status(result); // status-ok: per-artifact authenticode result line
    }
    Ok(())
}

/// Process a list of `SignConfig` entries against a set of artifacts, executing
/// the signing command for each matching artifact.  This is the shared
/// implementation behind both the `signs` and `binary_signs` top-level config
/// sections.
///
/// Signing commands are executed in parallel using `std::thread::scope` with
/// chunked parallelism (similar to the build stage), since each signing
/// invocation is an independent external process.
pub(crate) fn process_sign_configs(
    sign_configs: &[SignConfig],
    ctx: &mut Context,
    log: &StageLogger,
    filter_mode: ArtifactFilter,
    label: &str,
) -> Result<()> {
    let parallelism = std::cmp::max(
        1,
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4),
    );

    for (sign_idx, sign_cfg) in sign_configs.iter().enumerate() {
        let sub_label = sign_cfg
            .id
            .clone()
            .unwrap_or_else(|| format!("{}[{}]", label, sign_idx));

        // Evaluate the `if` conditional template — skip when rendered
        // result is falsy. Render failure hard-errors.
        let proceed = anodizer_core::config::evaluate_if_condition(
            sign_cfg.if_condition.as_deref(),
            &format!("{label} '{sub_label}'"),
            |t| ctx.render_template(t),
        )?;
        if !proceed {
            let reason = "`if` condition evaluated falsy".to_string();
            log.verbose(&format!(
                "skipped {} config '{}' — {}",
                label, sub_label, reason
            ));
            ctx.remember_skip(label, &sub_label, &reason);
            continue;
        }

        // Authenticode (Windows PE/MSI/DLL) signs IN PLACE via osslsigncode /
        // signtool — a wholly different lifecycle from the detached cosign/gpg
        // path below (derived argv, in-place mutation, no `.sig` artifact). It
        // carries its own `authenticode.artifacts` selector (default
        // `"windows"`), so it must branch out BEFORE the SignConfig-level
        // `artifacts` filter resolution — whose top-level default is `"none"`
        // and would otherwise skip an `authenticode: {}` config that never set
        // the outer `artifacts:` field.
        if let Some(authenticode) = &sign_cfg.authenticode {
            process_authenticode_config(
                authenticode,
                sign_cfg,
                ctx,
                log,
                label,
                &sub_label,
                parallelism,
            )?;
            continue;
        }

        let config_filter = sign_cfg.resolved_artifacts(match filter_mode {
            ArtifactFilter::FromConfig => SignConfig::DEFAULT_ARTIFACTS,
            ArtifactFilter::BinaryOnly => SignConfig::DEFAULT_ARTIFACTS_BINARY,
        });

        if sign_cfg.ids.as_ref().is_some_and(|ids| !ids.is_empty()) {
            if config_filter == "checksum" {
                log.warn("when artifacts is `checksum`, `ids` has no effect. ignoring");
            } else if config_filter == "source" {
                log.warn("when artifacts is `source`, `ids` has no effect. ignoring");
            }
        }

        if config_filter == "none" {
            log.verbose(&format!(
                "skipped {} config '{}' — `artifacts: none`",
                label, sub_label
            ));
            ctx.remember_skip(label, &sub_label, "artifacts: none");
            continue;
        }

        let cmd = sign_cfg
            .cmd
            .as_deref()
            .map(|s| s.to_string())
            .unwrap_or_else(default_sign_cmd);

        // Keyless cosign cannot run inside the determinism harness: cosign's
        // keyless mode needs ambient OIDC (Fulcio/Rekor), which the harness
        // strips for hermeticity, and a keyless config inherits the harness's
        // ephemeral `COSIGN_KEY` env (the `--key` flag is environment-bound),
        // crashing on `reading key: open $COSIGN_KEY: file name too long`.
        // Its signatures are non-deterministic and already drift-allowlisted,
        // so the harness skips it — exactly like the unavailable-tool / docker
        // / srpm skips above. A config with an explicit `--key` (anodizer's own
        // `--key=env://COSIGN_KEY`) signs with the ephemeral key and still runs.
        let args = harden_cosign_args_for_harness(&cmd, sign_cfg.resolved_args(), ctx);
        if is_keyless_cosign_under_harness(&cmd, &args, ctx) {
            let reason = KEYLESS_COSIGN_HARNESS_SKIP.to_string();
            log.verbose(&format!(
                "skipped {} config '{}' — {}",
                label, sub_label, reason
            ));
            ctx.remember_skip(label, &sub_label, &reason);
            continue;
        }

        if sign_cfg.args.as_ref().is_some_and(|a| a.is_empty()) {
            log.warn(&format!(
                "{} config has empty args — did you mean to omit args for defaults?",
                label
            ));
        }

        type ArtifactEntry = (
            std::path::PathBuf,
            String,
            std::collections::HashMap<String, String>,
            Option<String>,
            ArtifactKind,
        );
        let mut kind_matched = 0usize;
        let artifact_paths: Vec<ArtifactEntry> = {
            let mut matched = Vec::new();
            for a in ctx.artifacts.all().iter() {
                // The macOS `.app` directory bundle can never be cosign-blob /
                // gpg signed as a file — only the `.dmg`/`.pkg` wrapping it can.
                if anodizer_core::artifact::is_directory_bundle_artifact(a) {
                    continue;
                }
                match filter_mode {
                    ArtifactFilter::FromConfig => {
                        if !should_sign_artifact(a.kind, config_filter)? {
                            continue;
                        }
                    }
                    ArtifactFilter::BinaryOnly => {
                        if a.kind != ArtifactKind::Binary {
                            continue;
                        }
                    }
                }
                kind_matched += 1;
                if !crate::helpers::sign_ids_match(&a.metadata, sign_cfg.ids.as_ref()) {
                    continue;
                }
                matched.push((
                    a.path.clone(),
                    a.crate_name.clone(),
                    a.metadata.clone(),
                    a.target.clone(),
                    a.kind,
                ));
            }
            matched
        };

        if anodizer_core::artifact::ids_filter_eliminated_all(
            sign_cfg.ids.as_deref(),
            kind_matched,
            artifact_paths.len(),
        ) {
            log.warn(&format!(
                "ids filter {:?} on {} config '{}' matched no artifacts — \
                 this config will sign NOTHING",
                sign_cfg.ids.as_deref().unwrap_or(&[]),
                label,
                sub_label
            ));
        }

        let mut sign_jobs: Vec<SignJob> = Vec::new();

        let default_sig_template: &str = match filter_mode {
            ArtifactFilter::BinaryOnly => SignConfig::DEFAULT_BINARY_SIGNATURE_TEMPLATE,
            ArtifactFilter::FromConfig => SignConfig::DEFAULT_SIGNATURE_TEMPLATE,
        };

        for (
            artifact_path,
            artifact_crate_name,
            artifact_metadata,
            artifact_target,
            artifact_kind,
        ) in &artifact_paths
        {
            let artifact_str = artifact_path.to_string_lossy();
            let artifact_name = artifact_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            let artifact_id = artifact_metadata
                .get("id")
                .map(|s| s.as_str())
                .unwrap_or("");

            if matches!(filter_mode, ArtifactFilter::BinaryOnly) {
                if let Some(target) = artifact_target {
                    // The build-policy seeding: composite `Arch` from
                    // map_target plus the shared variant-var policy, with the
                    // amd64 micro-arch level read from the binary's real
                    // `amd64_variant` metadata (the key every producing stage
                    // writes) — a v3-tuned binary's signature/certificate
                    // template renders the same `{{ Amd64 }}` its own name
                    // was built from.
                    let (os, arch) = map_target(target);
                    let vars = ctx.template_vars_mut();
                    vars.set("Os", &os);
                    vars.set("Arch", &arch);
                    anodizer_core::archive_name::seed_variant_vars(
                        vars,
                        target,
                        artifact_metadata.get("amd64_variant").map(String::as_str),
                    );
                } else {
                    let vars = ctx.template_vars_mut();
                    vars.set("Os", "");
                    vars.set("Arch", "");
                    anodizer_core::archive_name::reset_variant_vars(vars);
                }
            }

            let signature_str =
                resolve_signature_path(sign_cfg, &artifact_str, ctx, default_sig_template)?;

            let certificate_str = sign_cfg
                .certificate
                .as_ref()
                .map(|tmpl| {
                    let preprocessed = tmpl
                        .replace("{{ .Artifact }}", &artifact_str)
                        .replace("{{ Artifact }}", &artifact_str);
                    ctx.render_template(&preprocessed).with_context(|| {
                        format!(
                            "sign: render certificate template '{}' for artifact {}",
                            tmpl, artifact_str
                        )
                    })
                })
                .transpose()?;

            let certificate_for_vars = certificate_str.clone();
            // Invariant: every value below is supplied by anodizer itself,
            // not by raw user input. Sources:
            //   - artifact / artifactName: stage-derived path / basename of
            //     an Artifact produced upstream (build/archive/etc.).
            //   - signature / certificate: rendered from sign-stage
            //     templates against the controlled template var set, then
            //     joined with a `dist/` prefix below if not already
            //     absolute.
            //   - digest / artifactID: read from artifact metadata, also
            //     populated by stages (no direct config write surface).
            // Values feed `Command::args` (no shell), so shell metacharacters
            // (`;`, backticks, `$()`) cannot escape into a subshell. Keep
            // this invariant in mind when adding new entries — anything
            // user-controllable that reaches argv must still be free of
            // path-traversal / option-injection risk.
            let shell_vars: HashMap<&str, &str> = HashMap::from([
                ("artifact", artifact_str.as_ref()),
                ("signature", signature_str.as_str()),
                ("certificate", certificate_for_vars.as_deref().unwrap_or("")),
                (
                    "digest",
                    artifact_metadata
                        .get("digest")
                        .map(|s| s.as_str())
                        .unwrap_or(""),
                ),
                ("artifactName", artifact_name),
                ("artifactID", artifact_id),
            ]);

            let signature_str = expand_shell_vars(&signature_str, &shell_vars);
            let certificate_str = certificate_str.map(|c| expand_shell_vars(&c, &shell_vars));

            let resolved = resolve_sign_args(
                &args,
                artifact_str.as_ref(),
                &signature_str,
                certificate_str.as_deref(),
            );

            // Empty rendered args (from conditional Tera blocks that
            // evaluated to "") are dropped — passing them to the signer
            // as empty positional args confuses gpg.
            let mut fully_resolved: Vec<String> = resolved
                .iter()
                .map(|arg| -> Result<Option<String>> {
                    let rendered = ctx
                        .render_template(arg)
                        .with_context(|| format!("sign: render {} arg '{}'", label, arg))?;
                    let expanded = expand_shell_vars(&rendered, &shell_vars);
                    if expanded.is_empty() {
                        Ok(None)
                    } else {
                        Ok(Some(expanded))
                    }
                })
                .filter_map(|r| r.transpose())
                .collect::<Result<Vec<_>>>()?;

            inject_gpg_faked_system_time(&cmd, &mut fully_resolved, ctx.env_source());

            let dist = &ctx.config.dist;
            let sig_path = {
                let resolved = std::path::PathBuf::from(&signature_str);
                if !resolved.starts_with(dist) {
                    dist.join(&resolved)
                } else {
                    resolved
                }
            };
            let is_binary_sign = matches!(filter_mode, ArtifactFilter::BinaryOnly);
            // Subject provenance: the signature inherits the signed
            // artifact's verdict record — transitively when the subject is
            // itself derived (signing an SBOM) — so the release `ids:`
            // filter gives it the same upload verdict as its subject.
            let (subject_kind_value, inherited_id) =
                anodizer_core::artifact::subject_verdict_record(*artifact_kind, artifact_metadata);
            let mut sig_metadata = std::collections::HashMap::new();
            sig_metadata.insert("type".to_string(), "Signature".to_string());
            if let Some(ref subject_kind) = subject_kind_value {
                sig_metadata.insert(
                    anodizer_core::artifact::SUBJECT_KIND_META.to_string(),
                    subject_kind.clone(),
                );
            }
            if let Some(ref subject_id) = inherited_id {
                sig_metadata.insert("id".to_string(), subject_id.clone());
            }
            if is_binary_sign {
                sig_metadata.insert("binary_sign".to_string(), "true".to_string());
            }
            let sig_name = sig_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| sig_path.display().to_string());
            // Per-target binary signatures live in per-target directories
            // (the preserved-bin layout keys on the directory, not the
            // basename), so their bare basenames collide across targets in
            // the registry. Register them under a target-qualified name —
            // the same way per-target archives embed their target — and
            // carry the triple on the artifact. The on-disk path is
            // untouched.
            let (sig_name, registered_target) = match artifact_target {
                Some(target) if is_binary_sign => (
                    qualify_basename_with_target(&sig_name, target),
                    Some(target.clone()),
                ),
                _ => (sig_name, None),
            };
            let mut job_artifacts = vec![anodizer_core::artifact::Artifact {
                kind: ArtifactKind::Signature,
                name: sig_name,
                path: sig_path,
                target: registered_target.clone(),
                crate_name: artifact_crate_name.clone(),
                metadata: sig_metadata,
                size: None,
            }];

            if let Some(ref cert_path_str) = certificate_str {
                let cert_resolved = std::path::PathBuf::from(cert_path_str);
                let cert_path = if !cert_resolved.starts_with(dist) {
                    dist.join(&cert_resolved)
                } else {
                    cert_resolved
                };
                let cert_name = cert_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| cert_path.display().to_string());
                let cert_name = match registered_target.as_deref() {
                    Some(target) => qualify_basename_with_target(&cert_name, target),
                    None => cert_name,
                };
                let mut cert_metadata = std::collections::HashMap::new();
                cert_metadata.insert("type".to_string(), "Certificate".to_string());
                if let Some(ref subject_kind) = subject_kind_value {
                    cert_metadata.insert(
                        anodizer_core::artifact::SUBJECT_KIND_META.to_string(),
                        subject_kind.clone(),
                    );
                }
                if let Some(ref subject_id) = inherited_id {
                    cert_metadata.insert("id".to_string(), subject_id.clone());
                }
                if is_binary_sign {
                    cert_metadata.insert("binary_sign".to_string(), "true".to_string());
                }
                job_artifacts.push(anodizer_core::artifact::Artifact {
                    kind: ArtifactKind::Certificate,
                    name: cert_name,
                    path: cert_path,
                    target: registered_target.clone(),
                    crate_name: artifact_crate_name.clone(),
                    metadata: cert_metadata,
                    size: None,
                });
            }

            if ctx.is_dry_run() {
                log.status(&format!(
                    "(dry-run) would run: {} {}",
                    cmd,
                    fully_resolved.join(" ")
                ));
                for artifact in job_artifacts {
                    ctx.artifacts.add(artifact);
                }
                continue;
            }

            let (_, stdin_data) = prepare_stdin_from(
                sign_cfg.stdin.as_deref(),
                sign_cfg.stdin_file.as_deref(),
                label,
            )?;

            let mut rendered_env: Vec<(String, String)> = sign_cfg
                .env
                .as_deref()
                .map(|env_list| {
                    anodizer_core::config::render_env_entries(env_list, |v| ctx.render_template(v))
                        .with_context(|| format!("sign[{label}]: render env entries"))
                })
                .transpose()?
                .unwrap_or_default();

            for (k, v) in shell_vars.iter() {
                if v.is_empty() {
                    continue;
                }
                if !rendered_env.iter().any(|(ek, _)| ek == *k) {
                    rendered_env.push(((*k).to_string(), (*v).to_string()));
                }
            }

            // cosign signing must never block on the sigstore consent prompt in
            // CI; export `COSIGN_YES` so the banner is suppressed. No-op for
            // gpg / other signers.
            ensure_cosign_consent_env(&cmd, &mut rendered_env);

            let rendered_env = if rendered_env.is_empty() {
                None
            } else {
                Some(rendered_env)
            };

            sign_jobs.push(SignJob {
                cmd: cmd.clone(),
                args: fully_resolved,
                stdin_data,
                env: rendered_env,
                label: label.to_string(),
                id_label: sign_cfg.resolved_id().to_string(),
                artifact_display: artifact_str.to_string(),
                signature_display: signature_str.clone(),
                output_flag: match sign_cfg.output.as_ref() {
                    Some(s) => s
                        .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                        .with_context(|| "sign: render output template")?,
                    None => false,
                },
                new_artifacts: job_artifacts,
                rename_after: None,
                authenticode_result: None,
                redact_extra: Vec::new(),
                env_remove: Vec::new(),
            });
        }

        if !sign_jobs.is_empty() {
            log.status(&format!(
                "signing {} artifacts with parallelism={}",
                sign_jobs.len(),
                parallelism
            ));
        }

        let mut all_new_artifacts: Vec<anodizer_core::artifact::Artifact> = Vec::new();

        let static_label = label_to_static(label);
        let verbosity = log.verbosity();
        let stage_name: &'static str = match static_label {
            "binary-sign" => "binary-sign",
            _ => "sign",
        };
        anodizer_core::parallel::run_parallel_chunks(&sign_jobs, parallelism, stage_name, |job| {
            let thread_log = anodizer_core::log::StageLogger::new(static_label, verbosity);
            execute_sign_job(job, &thread_log)
        })?;

        for job in &sign_jobs {
            all_new_artifacts.extend(job.new_artifacts.iter().cloned());
        }

        for artifact in all_new_artifacts {
            ctx.artifacts.add(artifact);
        }
    }

    if matches!(filter_mode, ArtifactFilter::BinaryOnly) {
        ctx.template_vars_mut().set("Os", "");
        ctx.template_vars_mut().set("Arch", "");
        ctx.template_vars_mut().set("Arm", "");
        ctx.template_vars_mut().set("Amd64", "");
        ctx.template_vars_mut().set("Mips", "");
    }

    Ok(())
}

/// Skip reason recorded when an Authenticode sign config has no resolvable
/// cert and `required` is false.
pub(crate) const AUTHENTICODE_NO_CERT_SKIP: &str =
    "no Authenticode cert resolved (set the cert env var or authenticode.cert_file)";

/// Drive one Authenticode (`authenticode:`) sign config: resolve the cert /
/// password / timestamp, select the Windows artifacts (kind + extension), build
/// the derived signer argv, and execute (or dry-run echo) in place.
///
/// In-place semantics diverge from the detached cosign/gpg path: the artifact
/// is mutated, NO `.sig`/`.pem` artifact is registered, and downstream
/// checksums/archives pick up the signed bytes. The cert path may come from a
/// literal `cert_file` (templated) or the env var named by `cert_env`; a
/// missing cert HARD-FAILS when `required`, else skips gracefully — the same
/// shape as the keyless-cosign-under-harness skip.
#[allow(clippy::too_many_arguments)]
fn process_authenticode_config(
    authenticode: &anodizer_core::config::AuthenticodeConfig,
    sign_cfg: &SignConfig,
    ctx: &mut Context,
    log: &StageLogger,
    label: &str,
    sub_label: &str,
    parallelism: usize,
) -> Result<()> {
    // Resolve the cert path: literal `cert_file` (templated) wins; otherwise
    // read the env var named by `resolved_cert_env()` (default
    // WINDOWS_CERT_FILE). The cert here is a PATH, never inline bytes.
    // The cert is a filesystem PATH; trim surrounding whitespace so a templated
    // or env-supplied value with a trailing newline still opens (the AUR-key
    // newline failure class). The password below is NOT trimmed — passphrase
    // whitespace may be significant.
    let cert_env_name = authenticode.resolved_cert_env().to_string();
    let cert_path: Option<String> = match authenticode.cert_file.as_deref() {
        Some(tmpl) => {
            let rendered = ctx
                .render_template(tmpl)
                .with_context(|| format!("sign[{sub_label}]: render authenticode cert_file"))?;
            let trimmed = rendered.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        None => ctx
            .env_var(&cert_env_name)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty()),
    };

    let Some(cert_path) = cert_path else {
        if authenticode.is_required() {
            anyhow::bail!(
                "{label} config '{sub_label}': authenticode is required but no cert resolved \
                 (set ${cert_env_name} to the .p12/.pfx path, or authenticode.cert_file)"
            );
        }
        log.verbose(&format!(
            "skipped {label} config '{sub_label}' — {AUTHENTICODE_NO_CERT_SKIP} (${cert_env_name})"
        ));
        ctx.remember_skip(label, sub_label, AUTHENTICODE_NO_CERT_SKIP);
        return Ok(());
    };

    // A configured-but-absent cert FILE is the same logical condition as an
    // absent cert env var — "no usable signing material" — so it must skip or
    // fail by the same `required` rule, not fall through to a generic
    // non-zero-exit from the signer. (Without this pre-check, `required: false`
    // would hard-error on osslsigncode/signtool failing to open the path,
    // inconsistent with the missing-env-var skip just above.) Dry-run previews
    // the command regardless — the cert need not exist yet to show what would
    // run — so the existence gate is live-mode only.
    if !ctx.is_dry_run() && !std::path::Path::new(&cert_path).exists() {
        if authenticode.is_required() {
            anyhow::bail!(
                "{label} config '{sub_label}': authenticode is required but the cert file \
                 '{cert_path}' does not exist"
            );
        }
        log.verbose(&format!(
            "skipped {label} config '{sub_label}' — {AUTHENTICODE_NO_CERT_SKIP} \
             (cert file '{cert_path}' not found)"
        ));
        ctx.remember_skip(label, sub_label, AUTHENTICODE_NO_CERT_SKIP);
        return Ok(());
    }

    // Password is optional (some certs carry none). Read it from the env var
    // named by `resolved_password_env()`; an empty value counts as absent.
    let password_env_name = authenticode.resolved_password_env().to_string();
    let password: Option<String> = ctx.env_var(&password_env_name).filter(|v| !v.is_empty());
    if password.is_none() {
        log.verbose(&format!(
            "{label} config '{sub_label}': no authenticode cert password (${password_env_name} \
             unset) — signing without -pass"
        ));
    }

    let tool = authenticode.resolved_tool().to_string();
    let timestamp_url = authenticode.resolved_timestamp_url().to_string();
    let url = authenticode.url.clone();

    // `name` defaults to the project name when unset; templated when set.
    let name: Option<String> = match authenticode.name.as_deref() {
        Some(tmpl) => {
            let rendered = ctx
                .render_template(tmpl)
                .with_context(|| format!("sign[{sub_label}]: render authenticode name"))?;
            (!rendered.trim().is_empty()).then_some(rendered)
        }
        None => {
            let project = ctx.config.project_name.clone();
            (!project.trim().is_empty()).then_some(project)
        }
    };

    // The `"windows"` selector kind-prefilters Binary/Installer/Library, then
    // refines by extension (.exe/.msi/.dll) where the path is in scope. The
    // per-artifact carry of crate_name/target means single-crate,
    // workspace-lockstep, and workspace per-crate all flow through unchanged.
    let config_filter = authenticode.resolved_artifacts();
    // Propagate a bad `authenticode.artifacts` filter rather than `.unwrap_or(false)`-ing
    // it to "matched nothing": an unrecognized selector must fail the stage, not silently
    // sign zero binaries and report success (which would ship unsigned .exe/.msi). Mirrors
    // the detached-signature path's `should_sign_artifact(...)?`.
    let mut matched: Vec<std::path::PathBuf> = Vec::new();
    for a in ctx.artifacts.all().iter() {
        if should_sign_artifact(a.kind, config_filter)?
            && windows_artifact_extension_matches(&a.path)
        {
            matched.push(a.path.clone());
        }
    }

    if matched.is_empty() {
        log.verbose(&format!(
            "{label} config '{sub_label}': authenticode matched no Windows artifacts (.exe/.msi/.dll)"
        ));
        return Ok(());
    }

    let mut jobs: Vec<SignJob> = Vec::new();
    for (job_idx, artifact_path) in matched.iter().enumerate() {
        let artifact_str = artifact_path.to_string_lossy().to_string();
        let artifact_base = artifact_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        let tool_base = std::path::Path::new(&tool)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&tool);
        let is_signtool = tool_base.starts_with("signtool");

        // osslsigncode requires a distinct `-out`; write to a sibling temp and
        // rename it over the original on success. signtool signs in place, so
        // it gets neither an `-out` token nor a rename. The temp name carries a
        // pid + per-job suffix so concurrent jobs (and future parallelism) can
        // never collide on a fixed `.authenticode-tmp` sibling. It stays in the
        // artifact's parent dir so the final rename is a same-filesystem move.
        let out_tmp: Option<std::path::PathBuf> = if is_signtool {
            None
        } else {
            let mut p = artifact_path.clone();
            let fname = format!(
                "{artifact_base}.{}.{job_idx}.authenticode-tmp",
                std::process::id()
            );
            p.set_file_name(fname);
            Some(p)
        };

        let args = build_authenticode_argv(
            &tool,
            &cert_path,
            password.as_deref(),
            &timestamp_url,
            name.as_deref(),
            url.as_deref(),
            &artifact_str,
            out_tmp
                .as_deref()
                .map(|p| p.to_string_lossy())
                .unwrap_or_default()
                .as_ref(),
        );

        let rename_after = out_tmp.map(|tmp| (tmp, artifact_path.clone()));

        if ctx.is_dry_run() {
            // dry-run mode is explicitly "show the user what would happen"; the
            // echo is masked so the cert password never reaches the log.
            log.status(&format!(
                "(dry-run) would run: {} {}",
                tool,
                redact_password_in_argv(&args)
            ));
            log.status(&format!("authenticode-signed {artifact_base}")); // status-ok: per-artifact authenticode result line
            continue;
        }

        // The cert password is passed in argv (`-pass` / `/p`), so the signer
        // needs nothing in its env. Two layers keep it out of the child env:
        //   1. `env_remove` strips the inherited `password_env` var so the
        //      child cannot read the secret from its environment at all.
        //   2. `redact_extra` scrubs the value from captured stdout/stderr
        //      under a synthetic key that always trips `is_secret`, so a tool
        //      echoing the argv on error is masked regardless of the
        //      user-chosen env-var name.
        let redact_extra: Vec<(String, String)> = password
            .as_ref()
            .map(|pw| vec![("AUTHENTICODE_PASSWORD".to_string(), pw.clone())])
            .unwrap_or_default();
        let env_remove: Vec<String> = if password.is_some() {
            vec![password_env_name.clone()]
        } else {
            Vec::new()
        };

        jobs.push(SignJob {
            cmd: tool.clone(),
            args,
            stdin_data: None,
            env: None,
            redact_extra,
            label: label.to_string(),
            id_label: sign_cfg.resolved_id().to_string(),
            artifact_display: artifact_str.clone(),
            signature_display: artifact_str.clone(),
            output_flag: match sign_cfg.output.as_ref() {
                Some(s) => s
                    .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                    .with_context(|| "sign: render output template")?,
                None => false,
            },
            // Authenticode signs IN PLACE — no detached signature/certificate
            // artifact is registered; the mutated artifact carries the
            // signature itself.
            new_artifacts: Vec::new(),
            rename_after,
            authenticode_result: Some(format!("authenticode-signed {artifact_base}")),
            env_remove,
        });
    }

    if jobs.is_empty() {
        return Ok(());
    }

    // `label_to_static` already collapses the runtime label to the canonical
    // `"sign"` / `"binary-sign"` static string, so reuse it for the parallel
    // stage name rather than re-matching the same two arms.
    let static_label = label_to_static(label);
    let verbosity = log.verbosity();
    anodizer_core::parallel::run_parallel_chunks(&jobs, parallelism, static_label, |job| {
        let thread_log = anodizer_core::log::StageLogger::new(static_label, verbosity);
        execute_sign_job(job, &thread_log)
    })?;

    Ok(())
}

/// Convert a runtime label string to a `&'static str` for `StageLogger::new`.
fn label_to_static(label: &str) -> &'static str {
    match label {
        "sign" => "sign",
        "binary-sign" => "binary-sign",
        _ => "sign",
    }
}

/// Inject `--faked-system-time=<SOURCE_DATE_EPOCH>!` after the first
/// arg when `cmd` is gpg and SDE is set, so the OpenPGP signature
/// packet's creation timestamp is pinned. With an EdDSA key this gives
/// byte-identical detached signatures across runs (RFC 8032). No-op if
/// the user already supplied `--faked-system-time`.
fn inject_gpg_faked_system_time(cmd: &str, args: &mut Vec<String>, env: &dyn EnvSource) {
    if cmd != "gpg" {
        return;
    }
    let Some(sde) = env.var("SOURCE_DATE_EPOCH") else {
        return;
    };
    if args
        .iter()
        .any(|a| a == "--faked-system-time" || a.starts_with("--faked-system-time="))
    {
        return;
    }
    let injection = format!("--faked-system-time={}!", sde);
    let insert_at = if args.is_empty() { 0 } else { 1 };
    args.insert(insert_at, injection);
}

#[cfg(test)]
mod faked_time_tests {
    use super::inject_gpg_faked_system_time;
    use anodizer_core::MapEnvSource;

    fn env_with_sde(value: &str) -> MapEnvSource {
        MapEnvSource::new().with("SOURCE_DATE_EPOCH", value)
    }

    fn env_without_sde() -> MapEnvSource {
        MapEnvSource::new()
    }

    #[test]
    fn injects_after_first_arg_for_gpg_with_sde() {
        let env = env_with_sde("1715000000");
        let mut args = vec![
            "--batch".into(),
            "--local-user".into(),
            "ABCD".into(),
            "--detach-sig".into(),
            "file".into(),
        ];
        inject_gpg_faked_system_time("gpg", &mut args, &env);
        assert_eq!(args[0], "--batch");
        assert_eq!(args[1], "--faked-system-time=1715000000!");
        assert_eq!(args[2], "--local-user");
    }

    #[test]
    fn no_inject_when_sde_unset() {
        let env = env_without_sde();
        let mut args = vec!["--batch".into(), "--detach-sig".into()];
        inject_gpg_faked_system_time("gpg", &mut args, &env);
        assert_eq!(args, vec!["--batch".to_string(), "--detach-sig".into()]);
    }

    #[test]
    fn no_inject_when_cmd_is_not_gpg() {
        let env = env_with_sde("1715000000");
        let mut args = vec!["sign-blob".into(), "--key=env://KEY".into()];
        inject_gpg_faked_system_time("cosign", &mut args, &env);
        assert_eq!(
            args,
            vec!["sign-blob".to_string(), "--key=env://KEY".into()]
        );
    }

    #[test]
    fn no_inject_when_user_already_passed_faked_system_time() {
        let env = env_with_sde("1715000000");
        let mut args = vec![
            "--batch".into(),
            "--faked-system-time=999!".into(),
            "--detach-sig".into(),
        ];
        inject_gpg_faked_system_time("gpg", &mut args, &env);
        let count = args
            .iter()
            .filter(|a| a.starts_with("--faked-system-time"))
            .count();
        assert_eq!(count, 1);
        assert_eq!(args[1], "--faked-system-time=999!");
    }

    #[test]
    fn injects_at_position_zero_when_args_empty() {
        let env = env_with_sde("42");
        let mut args: Vec<String> = vec![];
        inject_gpg_faked_system_time("gpg", &mut args, &env);
        assert_eq!(args, vec!["--faked-system-time=42!".to_string()]);
    }
}

#[cfg(test)]
mod harden_cosign_tests {
    use super::harden_cosign_args_for_harness;
    use anodizer_core::MapEnvSource;
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    /// Build a Context whose injected env carries (or omits) the harness marker.
    fn ctx_with_harness(harness: bool) -> Context {
        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        let env = if harness {
            MapEnvSource::new().with("ANODIZER_IN_DETERMINISM_HARNESS", "1")
        } else {
            MapEnvSource::new()
        };
        ctx.set_env_source(env);
        ctx
    }

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn appends_tlog_false_for_keyed_cosign_under_harness() {
        let ctx = ctx_with_harness(true);
        let out = harden_cosign_args_for_harness(
            "cosign",
            args(&[
                "sign-blob",
                "--key=env://COSIGN_KEY",
                "--bundle=cosign.bundle",
                "--yes",
                "artifact",
            ]),
            &ctx,
        );
        assert_eq!(out.last().map(String::as_str), Some("--tlog-upload=false"));
        assert_eq!(
            out.iter()
                .filter(|a| a.starts_with("--tlog-upload"))
                .count(),
            1,
            "appended exactly once: {out:?}"
        );
    }

    #[test]
    fn unchanged_when_not_under_harness() {
        let ctx = ctx_with_harness(false);
        let input = args(&["sign-blob", "--key=env://COSIGN_KEY", "artifact"]);
        let out = harden_cosign_args_for_harness("cosign", input.clone(), &ctx);
        assert_eq!(out, input);
    }

    #[test]
    fn unchanged_for_non_cosign_cmd() {
        let ctx = ctx_with_harness(true);
        let input = args(&["--detach-sig", "--key=secret", "artifact"]);
        let out = harden_cosign_args_for_harness("gpg", input.clone(), &ctx);
        assert_eq!(out, input);
    }

    #[test]
    fn unchanged_for_keyless_cosign() {
        let ctx = ctx_with_harness(true);
        let input = args(&["sign-blob", "--bundle=cosign.bundle", "--yes", "artifact"]);
        let out = harden_cosign_args_for_harness("cosign", input.clone(), &ctx);
        assert_eq!(out, input);
    }

    #[test]
    fn unchanged_when_tlog_already_pinned() {
        let ctx = ctx_with_harness(true);

        let eq_true = args(&["sign-blob", "--key=k", "--tlog-upload=true", "a"]);
        assert_eq!(
            harden_cosign_args_for_harness("cosign", eq_true.clone(), &ctx),
            eq_true
        );

        let two_token = args(&["sign-blob", "--key=k", "--tlog-upload", "false", "a"]);
        assert_eq!(
            harden_cosign_args_for_harness("cosign", two_token.clone(), &ctx),
            two_token
        );

        let eq_false = args(&["sign-blob", "--key=k", "--tlog-upload=false", "a"]);
        let out = harden_cosign_args_for_harness("cosign", eq_false, &ctx);
        assert_eq!(
            out.iter()
                .filter(|a| a.starts_with("--tlog-upload"))
                .count(),
            1,
            "no duplicate when already pinned false: {out:?}"
        );
    }

    #[test]
    fn matches_cosign_basename_through_path() {
        let ctx = ctx_with_harness(true);
        let out = harden_cosign_args_for_harness(
            "/usr/local/bin/cosign",
            args(&["sign-blob", "--key=env://COSIGN_KEY", "artifact"]),
            &ctx,
        );
        assert_eq!(out.last().map(String::as_str), Some("--tlog-upload=false"));
    }

    #[test]
    fn appends_tlog_false_for_two_token_key_form() {
        let ctx = ctx_with_harness(true);
        let out = harden_cosign_args_for_harness(
            "cosign",
            args(&[
                "sign-blob",
                "--key",
                "env://COSIGN_KEY",
                "--yes",
                "artifact",
            ]),
            &ctx,
        );
        assert_eq!(out.last().map(String::as_str), Some("--tlog-upload=false"));
    }
}
