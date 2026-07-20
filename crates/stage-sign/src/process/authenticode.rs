use super::*;

use anyhow::Result;

use anodizer_core::config::SignConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;

use crate::helpers::{
    build_authenticode_argv, redact_password_in_argv, should_sign_artifact,
    windows_artifact_extension_matches,
};

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
pub(crate) fn process_authenticode_config(
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
            // Authenticode embeds the signature in place via a different
            // trust model (PKCS#7 in the PE/MSI container); the detached
            // cosign/gpg verifiers do not apply.
            verify: None,
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
    anodizer_core::parallel::run_parallel_chunks(&jobs, parallelism, static_label, log, |job| {
        let thread_log = anodizer_core::log::StageLogger::new(static_label, verbosity);
        execute_sign_job(job, &thread_log)
    })?;

    Ok(())
}
