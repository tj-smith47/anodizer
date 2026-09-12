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
    archive_stem_for, binary_sign_asset_name, default_sign_cmd, expand_shell_vars,
    prepare_stdin_from, resolve_sign_args, resolve_signature_path, should_sign_artifact,
};

/// Process a list of `SignConfig` entries against a set of artifacts, executing
/// the signing command for each matching artifact.  This is the shared
/// implementation behind both the `signs` and `binary_signs` top-level config
/// sections.
///
/// Signing commands are executed in parallel via
/// [`anodizer_core::parallel::run_parallel_chunks`], bounded by
/// `ctx.options.parallelism` like every other subprocess-per-job stage, since
/// each signing invocation is an independent external process. Keyless cosign
/// is the exception: its invocations run one at a time regardless of
/// `--parallelism` (see the host TUF lock below).
pub(crate) fn process_sign_configs(
    sign_configs: &[SignConfig],
    ctx: &mut Context,
    log: &StageLogger,
    filter_mode: ArtifactFilter,
    label: &str,
) -> Result<()> {
    let parallelism = ctx.options.parallelism.max(1);

    'configs: for (sign_idx, sign_cfg) in sign_configs.iter().enumerate() {
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

        // The config's template argv. Everything that classifies the signer
        // — the harness skip, the verification mode, the host TUF lock — is
        // decided on the RENDERED per-job argv below, never on these
        // strings: a `--key` can arrive through a template.
        let args = sign_cfg.resolved_args();

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
        let mut absent_binaries = 0usize;
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
                        // A publish-only run rehydrates its registry from the
                        // preserved dist, which carries the release assets but
                        // not the raw cargo output the Binary entries point at
                        // (`<worktree>/.det-tmp/target/...`, outside `dist/`).
                        // Handing the signer a path that is not there would
                        // abort the publish, so those binaries drop out here
                        // and the config records a skip below.
                        if !ctx.is_dry_run() && !a.path.exists() {
                            absent_binaries += 1;
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

        // Every binary this config would have signed is absent from disk, so
        // it produces no signature at all. Record that as the config's skip:
        // the verify-release expectation derivation reads the same memento, so
        // the run does not go on to demand assets it just decided not to make.
        if artifact_paths.is_empty() && absent_binaries > 0 {
            let reason = format!("{absent_binaries} registered binary path(s) are not on disk");
            log.verbose(&format!(
                "skipped {} config '{}' — {}",
                label, sub_label, reason
            ));
            ctx.remember_skip(label, &sub_label, &reason);
            continue;
        }

        // A config that matched nothing renders no per-artifact argv, so the
        // harness skip inside the loop below never sees it; classify it once
        // from the config-level render so the skip is still recorded.
        if artifact_paths.is_empty()
            && is_keyless_cosign_under_harness(&cmd, &render_args_without_artifact(&args, ctx), ctx)
        {
            let reason = KEYLESS_COSIGN_HARNESS_SKIP.to_string();
            log.verbose(&format!(
                "skipped {} config '{}' — {}",
                label, sub_label, reason
            ));
            ctx.remember_skip(label, &sub_label, &reason);
            continue;
        }

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
            let signature_str = crate::helpers::dist_joined(&ctx.config.dist, &signature_str)
                .to_string_lossy()
                .into_owned();

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
            let certificate_str = certificate_str.map(|cert| {
                crate::helpers::dist_joined(&ctx.config.dist, &cert)
                    .to_string_lossy()
                    .into_owned()
            });

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
            let fully_resolved: Vec<String> = resolved
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
            let mut fully_resolved = harden_cosign_args_for_harness(&cmd, fully_resolved, ctx);

            // Keyless cosign cannot run inside the determinism harness:
            // cosign's keyless mode needs ambient OIDC (Fulcio/Rekor), which
            // the harness strips for hermeticity, and a keyless config
            // inherits the harness's ephemeral `COSIGN_KEY` env (the `--key`
            // flag is environment-bound), crashing on `reading key: open
            // $COSIGN_KEY: file name too long`. Its signatures are
            // non-deterministic and already drift-allowlisted, so the
            // harness skips the whole config — exactly like the
            // unavailable-tool / docker / srpm skips above. A config whose
            // rendered argv carries `--key` (anodizer's own
            // `--key=env://COSIGN_KEY`) signs with the ephemeral key and
            // still runs.
            if is_keyless_cosign_under_harness(&cmd, &fully_resolved, ctx) {
                let reason = KEYLESS_COSIGN_HARNESS_SKIP.to_string();
                log.verbose(&format!(
                    "skipped {} config '{}' — {}",
                    label, sub_label, reason
                ));
                ctx.remember_skip(label, &sub_label, &reason);
                continue 'configs;
            }

            inject_gpg_faked_system_time(&cmd, &mut fully_resolved, ctx.env_source());

            let sig_path = std::path::PathBuf::from(&signature_str);
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
            // the registry and on the release. Register them under the asset
            // name the target's archive was built from and carry the triple on
            // the artifact. The on-disk path is untouched.
            let archive_stem = if is_binary_sign {
                artifact_target.as_deref().and_then(|target| {
                    archive_stem_for(
                        ctx,
                        artifact_crate_name,
                        target,
                        anodizer_core::artifact::binary_name_of(
                            Some(*artifact_kind),
                            artifact_metadata,
                            artifact_path,
                        )
                        .as_deref(),
                    )
                })
            } else {
                None
            };
            let (sig_name, registered_target) = match artifact_target {
                Some(target) if is_binary_sign => (
                    binary_sign_asset_name(
                        &sig_name,
                        artifact_name,
                        archive_stem.as_deref(),
                        target,
                    ),
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
                let cert_path = std::path::PathBuf::from(cert_path_str);
                let cert_name = cert_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| cert_path.display().to_string());
                let cert_name = match registered_target.as_deref() {
                    Some(target) => binary_sign_asset_name(
                        &cert_name,
                        artifact_name,
                        archive_stem.as_deref(),
                        target,
                    ),
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
                certificate_display: certificate_str.clone(),
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
                verify: None,
            });
        }

        // Resolve the post-sign verification mode once per config, from the
        // argv the first job will actually spawn: the flags that classify a
        // signer (`--key`, `--bundle`, `--tlog-upload`) render identically
        // for every job of one config — templates only vary the artifact
        // paths — and a `--key` supplied through a template is visible only
        // in the rendered form. With no job (dry run, no matching artifact)
        // nothing is spawned, and the config-level render — the same one the
        // harness skip is classified from — stands in, so one config is
        // never classified two ways. Resolving once keeps the skip logged a
        // single time and the keyed public key derived a single time.
        let config_level_args: Vec<String>;
        let classified_args: &[String] = match sign_jobs.first() {
            Some(job) => &job.args,
            None => {
                config_level_args = render_args_without_artifact(&args, ctx);
                &config_level_args
            }
        };
        let verify_mode = crate::verify::resolve_config_verify_mode(
            sign_cfg.verify.as_ref(),
            &cmd,
            classified_args,
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

        // Keyed cosign verification needs the PUBLIC half of the signing
        // key: `cosign verify-blob --key` rejects a private key, so derive
        // it once per config via `cosign public-key --key <ref>` (the same
        // local, network-free load the preflight gate uses) into a temp
        // file that lives until the parallel fan-out below completes, under
        // the env the sign jobs run with so `env://VAR` refs resolve the
        // same way. A failed derivation is a hard error: the identical key
        // material would fail signing moments later anyway.
        let pubkey_file: Option<tempfile::NamedTempFile> = match (&verify_mode, sign_jobs.first()) {
            (crate::verify::ConfigVerifyMode::CosignKeyed { key_ref, .. }, Some(first)) => {
                let tmp = tempfile::Builder::new()
                    .prefix("anodizer-verify-")
                    .suffix(".pub")
                    .tempfile()
                    .context("sign verify: create temp file for derived public key")?;
                crate::verify::derive_cosign_public_key(
                    &cmd,
                    key_ref,
                    first.env.as_deref(),
                    tmp.path(),
                )?;
                Some(tmp)
            }
            _ => None,
        };
        let pubkey_path: Option<String> = pubkey_file
            .as_ref()
            .map(|f| f.path().to_string_lossy().into_owned());

        // Verify against the exact same artifact/signature strings the sign
        // argv used (they are cwd-relative in the same way), under the same
        // rendered env, with the same resolved binary.
        for job in &mut sign_jobs {
            job.verify = crate::verify::build_blob_verify_args(
                &verify_mode,
                &job.artifact_display,
                &job.signature_display,
                job.certificate_display.as_deref(),
                pubkey_path.as_deref(),
            )
            .map(|vargs| crate::verify::VerifyJob {
                cmd: cmd.clone(),
                args: vargs,
                env: job.env.clone(),
                what: job.artifact_display.clone(),
            });
        }

        // Keyless cosign contends a host-exclusive sigstore TUF trust store:
        // two concurrent invocations on one host collide and the loser exits
        // with `creating cached local store: resource temporarily
        // unavailable`, whether or not the store is already populated. So a
        // keyless config runs one invocation at a time and holds the
        // host-level advisory lock for its whole run, which also queues a
        // second anodizer process behind this one instead of racing it.
        // Keyed cosign (`--key=`) never contacts Fulcio/Rekor and keeps the
        // full parallelism. Decided per job on the argv that job spawns —
        // one keyless job makes the config keyless.
        let keyless = sign_jobs
            .iter()
            .any(|job| is_keyless_cosign(&job.cmd, &job.args));
        let tuf_init_locks = if keyless {
            log.verbose(&format!(
                "keyless cosign: serializing {} invocation(s) — concurrent invocations \
                 collide on the sigstore TUF trust store",
                sign_jobs.len()
            ));
            // Each cache dir must be resolved from the env the cosign CHILD
            // sees: a config's rendered `env:` entries can set TUF_ROOT (or
            // HOME) and shadow the process env, and a templated TUF_ROOT can
            // resolve differently per job — every distinct store gets its own
            // lock, so none of them is left racing a sibling process.
            let job_envs: Vec<&[(String, String)]> = sign_jobs
                .iter()
                .map(|job| job.env.as_deref().unwrap_or(&[]))
                .collect();
            crate::tuf_cache::keyless_cosign_host_locks(&job_envs, ctx.env_source(), log)
        } else {
            Vec::new()
        };
        let effective_parallelism = if keyless { 1 } else { parallelism };

        if !sign_jobs.is_empty() {
            log.status(&format!(
                "signing {} artifacts with parallelism={}",
                sign_jobs.len(),
                effective_parallelism
            ));
        }

        let mut all_new_artifacts: Vec<anodizer_core::artifact::Artifact> = Vec::new();

        let static_label = label_to_static(label);
        let stage_name: &'static str = match static_label {
            "binary-sign" => "binary-sign",
            _ => "sign",
        };
        // cosign is the network-dependent signer (Fulcio/Rekor/TUF CDN), so
        // its failures are retried; local signers (gpg, osslsigncode) fail
        // deterministically and keep the single fast attempt.
        let run_job = |job: &SignJob| {
            let thread_log = log.with_stage(static_label);
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
            // Verification runs after the sign in the same worker, so a
            // keyless config's host TUF lock covers its cosign verify too.
            // A bad signature is a deterministic failure — `retry_transient`
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
        anodizer_core::parallel::run_parallel_chunks(
            &sign_jobs,
            effective_parallelism,
            stage_name,
            log,
            run_job,
        )?;
        drop(tuf_init_locks);

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
