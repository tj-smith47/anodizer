use super::*;

use std::collections::HashMap;

use anyhow::Result;

use anodizer_core::EnvSource;
use anodizer_core::artifact::ArtifactKind;
use anodizer_core::config::SignConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::target::map_target;

use crate::helpers::{
    default_sign_cmd, expand_shell_vars, prepare_stdin_from, resolve_sign_args,
    resolve_signature_path, should_sign_artifact,
};

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

/// Process a list of `SignConfig` entries against a set of artifacts, executing
/// the signing command for each matching artifact.  This is the shared
/// implementation behind both the `signs` and `binary_signs` top-level config
/// sections.
///
/// Signing commands are executed in parallel via
/// [`anodizer_core::parallel::run_parallel_chunks`], bounded by
/// `ctx.options.parallelism` like every other subprocess-per-job stage, since
/// each signing invocation is an independent external process. Keyless cosign
/// fan-outs sign their first artifact alone before parallelizing (see the
/// TUF warm-up below).
pub(crate) fn process_sign_configs(
    sign_configs: &[SignConfig],
    ctx: &mut Context,
    log: &StageLogger,
    filter_mode: ArtifactFilter,
    label: &str,
) -> Result<()> {
    let parallelism = ctx.options.parallelism.max(1);

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
            ArtifactFilter::FromConfig | ArtifactFilter::CombinedChecksumOnly => {
                SignConfig::DEFAULT_ARTIFACTS
            }
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

        // Resolve the post-sign verification mode once per config (the
        // discriminating inputs — cmd, raw argv, identity env — are all
        // config-level), so a skip is logged a single time and the keyed
        // public key is derived a single time.
        let verify_mode = crate::verify::resolve_config_verify_mode(
            sign_cfg.verify.as_ref(),
            &cmd,
            &args,
            sign_cfg.certificate.is_some(),
            ctx.env_source(),
        );
        match &verify_mode {
            crate::verify::ConfigVerifyMode::Disabled => log.verbose(&format!(
                "{} config '{}': signature verification disabled by `verify.enabled: false`",
                label, sub_label
            )),
            crate::verify::ConfigVerifyMode::Skip(reason) => log.verbose(&format!(
                "{} config '{}': skipping signature verification — {}",
                label, sub_label, reason
            )),
            _ => {}
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
                    ArtifactFilter::CombinedChecksumOnly => {
                        // Honor the config's own filter (so a config that does
                        // not select checksums signs nothing here), then narrow
                        // to the COMBINED checksums file — the only artifact
                        // `refresh_combined_checksums` rewrites, hence the only
                        // signature that went stale. Split `.sha256` sidecars
                        // are never rewritten, so their signatures stay valid.
                        if !should_sign_artifact(a.kind, config_filter)? {
                            continue;
                        }
                        let is_combined = a
                            .metadata
                            .get(anodizer_core::artifact::COMBINED_CHECKSUM_META)
                            .map(String::as_str)
                            == Some(anodizer_core::artifact::COMBINED_CHECKSUM_VALUE);
                        if !is_combined {
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

        // Keyed cosign verification needs the PUBLIC half of the signing
        // key: `cosign verify-blob --key` rejects a private key, so derive
        // it once per config via `cosign public-key --key <ref>` (the same
        // local, network-free load the preflight gate uses) into a temp
        // file that lives until the parallel fan-out below completes. A
        // failed derivation is a hard error: the identical key material
        // would fail signing moments later anyway.
        let pubkey_file: Option<tempfile::NamedTempFile> = match &verify_mode {
            crate::verify::ConfigVerifyMode::CosignKeyed { key_ref, .. }
                if !ctx.is_dry_run() && !artifact_paths.is_empty() =>
            {
                let derive_env: Vec<(String, String)> = sign_cfg
                    .env
                    .as_deref()
                    .map(|env_list| {
                        anodizer_core::config::render_env_entries(env_list, |v| {
                            ctx.render_template(v)
                        })
                        .with_context(|| format!("sign[{label}]: render env entries"))
                    })
                    .transpose()?
                    .unwrap_or_default();
                let tmp = tempfile::Builder::new()
                    .prefix("anodizer-verify-")
                    .suffix(".pub")
                    .tempfile()
                    .context("sign verify: create temp file for derived public key")?;
                crate::verify::derive_cosign_public_key(
                    &cmd,
                    key_ref,
                    Some(&derive_env),
                    tmp.path(),
                )?;
                Some(tmp)
            }
            _ => None,
        };
        let pubkey_path: Option<String> = pubkey_file
            .as_ref()
            .map(|f| f.path().to_string_lossy().into_owned());

        let mut sign_jobs: Vec<SignJob> = Vec::new();

        let default_sig_template: &str = match filter_mode {
            ArtifactFilter::BinaryOnly => SignConfig::DEFAULT_BINARY_SIGNATURE_TEMPLATE,
            ArtifactFilter::FromConfig | ArtifactFilter::CombinedChecksumOnly => {
                SignConfig::DEFAULT_SIGNATURE_TEMPLATE
            }
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

            // Render `stdin` through the template engine (then shell-var
            // expansion, mirroring the args path above) so a passphrase like
            // `{{ Env.GPG_PASSPHRASE }}` reaches the signer as its value, not
            // the literal template string. `stdin_file` is a path read raw.
            let rendered_stdin = match sign_cfg.stdin.as_deref() {
                Some(s) => Some(expand_shell_vars(
                    &ctx.render_template(s)
                        .with_context(|| format!("sign: render {label} stdin"))?,
                    &shell_vars,
                )),
                None => None,
            };
            let (_, stdin_data) = prepare_stdin_from(
                rendered_stdin.as_deref(),
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

            // Verify against the exact same artifact/signature strings the
            // sign argv used (they are cwd-relative in the same way), under
            // the same rendered env, with the same resolved binary.
            let verify_job = crate::verify::build_blob_verify_args(
                &verify_mode,
                artifact_str.as_ref(),
                &signature_str,
                certificate_str.as_deref(),
                pubkey_path.as_deref(),
            )
            .map(|vargs| crate::verify::VerifyJob {
                cmd: cmd.clone(),
                args: vargs,
                env: rendered_env.clone(),
                what: artifact_str.to_string(),
            });

            // Re-signing a combined checksum: the sign stage already wrote
            // these `.sig`/`.pem` files over the pre-refresh bytes. The default
            // `gpg --output <sig> --detach-sig` refuses to overwrite a file
            // already on disk without a tty (exit 2, leaving the stale
            // signature in place), so clear the stale sidecars first — the
            // reused signer then writes a fresh signature over the refreshed
            // bytes, byte-identical to a cold first sign.
            if matches!(filter_mode, ArtifactFilter::CombinedChecksumOnly) {
                for produced in &job_artifacts {
                    if produced.path.exists() {
                        let _ = std::fs::remove_file(&produced.path);
                    }
                }
            }

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
                verify: verify_job,
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
        // cosign is the network-dependent signer (Fulcio/Rekor/TUF CDN), so
        // its failures are retried; local signers (gpg, osslsigncode) fail
        // deterministically and keep the single fast attempt.
        let run_job = |job: &SignJob| {
            let thread_log = anodizer_core::log::StageLogger::new(static_label, verbosity);
            if is_cosign_cmd(&job.cmd) {
                retry_transient(
                    &COSIGN_TRANSIENT_RETRY,
                    &thread_log,
                    &job.artifact_display,
                    &mut || execute_sign_job(job, &thread_log),
                )?;
            } else {
                execute_sign_job(job, &thread_log)?;
            }
            // Verification runs after the sign in the same worker, so the
            // keyless TUF warm-up below covers it too: the first job's
            // verify completes serially before the parallel fan-out. A bad
            // signature is a deterministic failure — `retry_transient`
            // fast-fails it via `is_deterministic_sign_failure` — while the
            // ladder still absorbs the transient network/TUF class a
            // tlog-checking cosign verify can hit.
            if let Some(v) = &job.verify {
                if is_cosign_cmd(&v.cmd) {
                    retry_transient(
                        &COSIGN_TRANSIENT_RETRY,
                        &thread_log,
                        &format!("verification of {}", v.what),
                        &mut || crate::verify::execute_verify_job(v, &thread_log),
                    )?;
                } else {
                    crate::verify::execute_verify_job(v, &thread_log)?;
                }
            }
            Ok(())
        };
        // Keyless cosign lazily initializes the TUF trust root (default
        // `~/.sigstore/root`, `TUF_ROOT` override) under an exclusive flock
        // on its FIRST run per host. Fanning out onto a cold cache makes
        // every first-wave worker race that lock and the losers die with
        // `creating cached local store: resource temporarily unavailable`
        // (flock EAGAIN). On a cold cache: hold a host-level advisory lock —
        // so a second anodizer process on the same host can't drive a
        // parallel cold init — and sign one artifact alone to warm the
        // cache, then parallelize the rest. On a warm cache the init is a
        // no-op, so the serialized first sign is skipped.
        //
        // The lock is taken BEFORE the warm probe: an unlocked probe can
        // observe a cache another process is mid-initializing (root.json and
        // a first target already written, the rest in flight under cosign's
        // internal flock), classify it warm, and fan out straight into the
        // race. Acquiring first means a warm verdict is only reached after
        // any in-flight init finished; an uncontended acquire costs
        // microseconds, so warm runs barely pay for it.
        let mut tuf_init_lock: Option<crate::tuf_cache::TufInitLock> = None;
        let parallel_jobs = if is_keyless_cosign(&cmd, &args) && !sign_jobs.is_empty() {
            // The cache dir must be resolved from the env the cosign CHILD
            // sees: the sign config's rendered `env:` entries can set
            // TUF_ROOT (or HOME) and shadow the process env. Job 0 carries
            // that rendered env, and it is also the invocation that would
            // drive the init.
            let overlay: &[(String, String)] = sign_jobs[0].env.as_deref().unwrap_or(&[]);
            let cache_dir = crate::tuf_cache::tuf_cache_dir(overlay, ctx.env_source());
            if let Some(dir) = cache_dir.as_deref() {
                match crate::tuf_cache::TufInitLock::acquire(dir) {
                    Ok(lock) => tuf_init_lock = Some(lock),
                    // Signing must not fail on lock plumbing: degrade to the
                    // process-local serialization below (and a best-effort
                    // unlocked warm probe).
                    Err(err) => log.verbose(&format!(
                        "could not acquire host-level TUF init lock ({err:#}); \
                         falling back to process-local serialization only"
                    )),
                }
            }
            if cache_dir
                .as_deref()
                .is_some_and(crate::tuf_cache::tuf_cache_is_warm)
            {
                // Warm needs no init: release immediately so other processes
                // stop queueing behind this run's fan-out.
                tuf_init_lock = None;
                log.verbose(
                    "keyless cosign: sigstore TUF trust root already cached; \
                     skipping serialized warm-up",
                );
                &sign_jobs[..]
            } else if sign_jobs.len() > 1 {
                log.verbose(
                    "keyless cosign: signing first artifact serially to initialize \
                     the sigstore TUF trust root before parallel fan-out",
                );
                run_job(&sign_jobs[0])?;
                // Init is complete after the first sign; release so other
                // processes stop queueing behind the fan-out.
                tuf_init_lock = None;
                &sign_jobs[1..]
            } else {
                // A single job IS the initializing invocation; the lock is
                // held across the parallel runner (one job) and dropped after.
                &sign_jobs[..]
            }
        } else {
            &sign_jobs[..]
        };
        anodizer_core::parallel::run_parallel_chunks(
            parallel_jobs,
            parallelism,
            stage_name,
            log,
            run_job,
        )?;
        drop(tuf_init_lock);

        let verified = sign_jobs.iter().filter(|j| j.verify.is_some()).count();
        if verified > 0 {
            // Reaching here means every verify job exited 0 (a failure
            // propagates out of the parallel runner above).
            log.status(&format!("verified {verified} signature(s)")); // status-ok: per-config verification result
        }
        drop(pubkey_file);

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

/// Inject `--faked-system-time=<SOURCE_DATE_EPOCH>!` after the first
/// arg when `cmd` is gpg and SDE is set, so the OpenPGP signature
/// packet's creation timestamp is pinned. With an EdDSA key this gives
/// byte-identical detached signatures across runs (RFC 8032). No-op if
/// the user already supplied `--faked-system-time`.
pub(crate) fn inject_gpg_faked_system_time(cmd: &str, args: &mut Vec<String>, env: &dyn EnvSource) {
    if !anodizer_core::signing::is_gpg_command(cmd) {
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
