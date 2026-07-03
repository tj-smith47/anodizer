use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result, bail};

use anodizer_core::artifact::ArtifactKind;
use anodizer_core::context::Context;
use anodizer_core::stage::Stage;

mod expected;
mod helpers;
mod keyload;
mod process;

pub use expected::expected_signature_assets;
pub use helpers::VALID_SIGN_ARTIFACT_FILTERS;
pub use keyload::{CosignKeyLoad, verify_cosign_key_loads};

use helpers::{default_sign_cmd, prepare_stdin_from, resolve_sign_args, validate_sign_config_ids};
use process::{ArtifactFilter, process_sign_configs};

// Helpers (should_sign_artifact, resolve_signature_path, prepare_stdin_from,
// default_sign_cmd, expand_shell_vars, resolve_sign_args) live in `helpers.rs`.

// Shared sign processing (ArtifactFilter, SignJob, execute_sign_job,
// process_sign_configs, label_to_static) lives in `process.rs`.

/// Publishers that consume the `signs:` stage's detached signature /
/// certificate artifacts in publish-only mode.
///
/// `github-release` and `blob` upload them via `release_uploadable_kinds`;
/// `artifactory` and `uploads` upload them when an entry sets
/// `signature: true`. The `signs:` loop self-skips only when EVERY one of
/// these is deselected (see [`signs_fully_deselected`]), so a selected
/// signature consumer can never be starved of the sidecars it was
/// configured to publish. This is the single source of truth shared by the
/// runtime ([`SignStage::run`]) and `anodizer preflight`; adding or removing
/// a consumer is a one-line edit here that both sites pick up.
pub fn signs_consumers() -> &'static [&'static str] {
    &["github-release", "blob", "artifactory", "uploads"]
}

/// True when no selected publisher consumes `signs:` output, so the
/// `signs:` loop has no downstream sink and can be skipped.
///
/// An empty `--publishers` allowlist deselects nothing, so this returns
/// `false` (the loop runs); it returns `true` only when EVERY built-in
/// publisher in [`signs_consumers`] is deselected AND no *selected* custom
/// publisher (`config.publishers`) opts into signature uploads
/// (`signature: true`). A custom publisher with `signature: true` is a fifth
/// kind of `signs:` consumer — one with a user-defined name that cannot live
/// in the static [`signs_consumers`] list — so targeting it
/// (`--publishers my-cdn`) must keep `signs:` alive even though every built-in
/// consumer is deselected. A nameless custom entry resolves to its index label
/// for the deselection check, matching `select_custom_publishers`.
pub fn signs_fully_deselected(ctx: &Context) -> bool {
    let builtins_deselected = signs_consumers()
        .iter()
        .all(|p| ctx.publisher_deselected(p));
    if !builtins_deselected {
        return false;
    }
    let custom_signature_selected = ctx
        .config
        .publishers
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .enumerate()
        .any(|(i, p)| {
            p.signature == Some(true) && {
                let name = p.name.clone().unwrap_or_else(|| format!("publisher[{i}]"));
                !ctx.publisher_deselected(&name)
            }
        });
    !custom_signature_selected
}

/// True when the `binary_signs:` slice has no live work in this run, so the
/// stage skips it and `anodizer preflight` omits its env requirements.
///
/// Unlike `signs:` (gated on its consumer set), `binary_signs:` signs the raw
/// binaries and embeds those signatures at BUILD time; its
/// `Signature`/`Certificate` outputs carry the `binary_sign` marker and are
/// filtered out of EVERY publish-time consumer (`is_binary_sign_output` —
/// github/gitlab/gitea release upload, blob, artifactory, the generic
/// per-publisher signature opt-in, attest subjects) and are excluded from the
/// `expected_signature_assets` set verify-release checks. So in publish-only
/// mode the loop produces only discarded work while demanding cosign/GPG
/// material the publish-time runner does not carry. The full `anodizer build`
/// / `anodizer release` pipeline (`is_publish_only() == false`) still runs it,
/// so the binaries that ship are still signed.
///
/// This is the single source of truth shared by the runtime
/// ([`SignStage::run`]) and `anodizer preflight`, the same discipline as
/// [`signs_fully_deselected`].
pub fn binary_signs_skipped(ctx: &Context) -> bool {
    ctx.is_publish_only()
}

// ---------------------------------------------------------------------------
// SignStage
// ---------------------------------------------------------------------------

/// Sign stage: signs artifacts using GPG, cosign, or other signing tools.
///
/// Calls `ctx.refresh_artifacts_var()` after all signing completes, matching
/// the artifact registry is refreshed. This ensures newly-added signature
/// and certificate artifacts are visible to downstream stages.
pub struct SignStage;

/// Binary-only signing stage used by `anodizer build`. Selects the
/// `sign.BinaryPipe` — runs the `binary_signs` loop but skips the generic
/// `signs` loop, which at build-time would see only binaries anyway but
/// with the wrong semantics (a user with `signs: [{artifacts: all}]`
/// doesn't expect signing to happen during `anodizer build`).
pub struct BinarySignStage;

impl Stage for BinarySignStage {
    fn name(&self) -> &str {
        "binary-sign"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("binary-sign");
        // Validate binary_signs IDs — same check SignStage does.
        validate_sign_config_ids(&ctx.config.binary_signs, "binary-sign", "binary_signs")?;
        let binary_sign_configs = ctx.config.binary_signs.clone();
        process_sign_configs(
            &binary_sign_configs,
            ctx,
            &log,
            ArtifactFilter::BinaryOnly,
            "binary-sign",
        )?;
        ctx.refresh_artifacts_var();
        Ok(())
    }
}

impl Stage for SignStage {
    fn name(&self) -> &str {
        "sign"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("sign");

        // Validate sign config IDs (uniqueness + reserved-label collision).
        validate_sign_config_ids(&ctx.config.signs, "sign", "signs")?;
        validate_sign_config_ids(&ctx.config.binary_signs, "binary-sign", "binary_signs")?;

        // ----------------------------------------------------------------
        // GPG / generic signing via `signs` config (supports multiple)
        // ----------------------------------------------------------------
        //
        // Consumer-selection gate (scoped to the `signs:` slice only). `sign`
        // is a PREP stage, not a publisher, so it has no `--publishers`
        // identity of its own. Its `signs:` output — detached signatures and
        // certificates over archives/checksums — is consumed by the publishers
        // in `signs_consumers()`. When EVERY one of those consumers is
        // deselected (e.g. `--publishers npm`), no selected surface will ever
        // read these signatures, so producing them would demand cosign/GPG
        // material a publisher-scoped runner does not carry for no downstream
        // effect. Skip the `signs:` loop in that case. The consumer set is the
        // single source of truth in `signs_consumers()`, so the runtime gate
        // and the preflight gate can never diverge.
        //
        // `binary_signs:` is deliberately NOT gated here: it is the
        // build-time binary-signing selection (a different consumer model —
        // the signed binaries flow into archives/installers, not detached
        // sidecars), so it keeps running below regardless of the publish-time
        // allowlist. An EMPTY allowlist deselects nothing, so the main release
        // job still signs.
        let sign_configs = if signs_fully_deselected(ctx) {
            if !ctx.config.signs.is_empty() {
                log.status(&format!(
                    "skipped signs — every consumer ({}) is deselected",
                    signs_consumers().join(" / "),
                ));
            }
            Vec::new()
        } else {
            ctx.config.signs.clone()
        };
        process_sign_configs(&sign_configs, ctx, &log, ArtifactFilter::FromConfig, "sign")?;

        // ----------------------------------------------------------------
        // Binary-specific signing via `binary_signs` config
        // Same as `signs` but always filters to Binary artifacts only.
        // ----------------------------------------------------------------
        //
        // Publish-only gate (scoped to the `binary_signs:` slice only).
        // `binary_signs:` signs the raw BINARIES and its signatures are
        // embedded into archives at BUILD time; the `Signature`/`Certificate`
        // outputs carry the `binary_sign` marker and are filtered out of every
        // publish-time consumer (`is_binary_sign_output`). In `--publish-only`
        // the source binaries are not even rebuilt, so this loop would only
        // produce discarded sidecars while demanding cosign/GPG material the
        // publish-time runner does not carry. Skip it there. The full build /
        // release pipeline (`is_publish_only() == false`) still runs it, so the
        // shipped binaries are still signed. `binary_signs_skipped` is the
        // single source of truth shared with `anodizer preflight`.
        if binary_signs_skipped(ctx) {
            if !ctx.config.binary_signs.is_empty() {
                log.verbose("skipped binary_signs — publish-only mode (binary signing is a build-time concern; its output has no publish-time consumer)");
            }
        } else {
            let binary_sign_configs = ctx.config.binary_signs.clone();
            process_sign_configs(
                &binary_sign_configs,
                ctx,
                &log,
                ArtifactFilter::BinaryOnly,
                "binary-sign",
            )?;
        }

        // Refresh the artifacts template variable so newly-added signatures
        // and certificates are visible to downstream stages (matching
        // refresh the artifact registry).
        ctx.refresh_artifacts_var();
        Ok(())
    }
}

/// Pipeline stage for signing Docker images via `docker_signs` config.
/// Must run after `DockerStage` so Docker image artifacts are present.
pub struct DockerSignStage;

impl Stage for DockerSignStage {
    fn name(&self) -> &str {
        "docker-sign"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("docker-sign");

        // Operator-selection gate. DockerSignStage runs `cosign sign` and
        // PUSHES signatures to the OCI registry (an external, irreversible
        // publish) but runs as a pipeline stage OUTSIDE the trait-based
        // dispatch chokepoint, so the uniform `--skip` / `--publishers` filter
        // does not reach it. Consult `publisher_deselected("docker-sign")` so
        // an operator who ran `--publishers cargo` (or `--skip=docker-sign`)
        // does NOT push signatures. Like its other skip paths, docker-sign
        // records no `publish_report` row — but the skip is never silent.
        if ctx.publisher_deselected("docker-sign") {
            log.status(&ctx.deselected_reason("docker-sign"));
            return Ok(());
        }

        // ----------------------------------------------------------------
        // Docker image signing via `docker_signs` config
        // ----------------------------------------------------------------
        if let Some(docker_signs) = ctx.config.docker_signs.clone() {
            // Validate docker_signs IDs are unique.
            {
                let mut seen_docker = std::collections::HashSet::new();
                for cfg in &docker_signs {
                    let id = cfg.resolved_id();
                    if !seen_docker.insert(id.to_string()) {
                        anyhow::bail!("found 2 docker_signs with the ID '{}'", id);
                    }
                }
            }

            for docker_sign_cfg in &docker_signs {
                let sign_id = docker_sign_cfg.resolved_id();

                // Evaluate the `if` conditional template for docker signs.
                // Hard-fail on render error: silent skip would ship unsigned
                // images.
                let proceed = anodizer_core::config::evaluate_if_condition(
                    docker_sign_cfg.if_condition.as_deref(),
                    &format!("docker-sign '{sign_id}'"),
                    |t| ctx.render_template(t),
                )?;
                if !proceed {
                    let reason = "`if` condition evaluated falsy".to_string();
                    log.verbose(&format!(
                        "skipped docker-sign config '{}' — {}",
                        sign_id, reason
                    ));
                    ctx.remember_skip("docker-sign", sign_id, &reason);
                    continue;
                }

                let cmd = docker_sign_cfg.resolved_cmd().to_string();

                let args = crate::process::harden_cosign_args_for_harness(
                    &cmd,
                    docker_sign_cfg.resolved_args(),
                    ctx,
                );

                // Keyless cosign cannot run inside the determinism harness (no
                // ambient OIDC; the ephemeral `COSIGN_KEY` env crashes a `--key`-
                // less invocation). Mirror the `signs`/`binary_signs` skip so the
                // discriminator (cmd==cosign + no `--key`) is uniform across
                // every sign family. A `--key`-bearing config still runs.
                if crate::process::is_keyless_cosign_under_harness(&cmd, &args, ctx) {
                    let reason = crate::process::KEYLESS_COSIGN_HARNESS_SKIP.to_string();
                    log.verbose(&format!(
                        "skipped docker-sign config '{}' — {}",
                        sign_id, reason
                    ));
                    ctx.remember_skip("docker-sign", sign_id, &reason);
                    continue;
                }

                let docker_filter = docker_sign_cfg.resolved_artifacts();

                if docker_filter == "none" {
                    log.verbose(&format!(
                        "skipped docker-sign config '{}' — `artifacts: none`",
                        sign_id
                    ));
                    ctx.remember_skip("docker-sign", sign_id, "artifacts: none");
                    continue;
                }

                // Collect docker artifacts based on the filter mode.
                // DockerImageV2 is included in all filter modes:
                // "images" → DockerImage + DockerImageV2
                // "manifests" → DockerManifest + DockerImageV2
                // "all" → DockerImage + DockerManifest + DockerImageV2
                // "" (default) → DockerImageV2 only
                // "none" → nothing (handled above)
                let docker_artifacts: Vec<_> = match docker_filter {
                    "images" => {
                        let mut arts = ctx.artifacts.by_kind(ArtifactKind::DockerImage);
                        arts.extend(ctx.artifacts.by_kind(ArtifactKind::DockerImageV2));
                        arts
                    }
                    "" => ctx.artifacts.by_kind(ArtifactKind::DockerImageV2),
                    "manifests" => {
                        let mut arts = ctx.artifacts.by_kind(ArtifactKind::DockerManifest);
                        arts.extend(ctx.artifacts.by_kind(ArtifactKind::DockerImageV2));
                        arts
                    }
                    "all" => {
                        let mut arts = ctx.artifacts.by_kind(ArtifactKind::DockerImage);
                        arts.extend(ctx.artifacts.by_kind(ArtifactKind::DockerManifest));
                        arts.extend(ctx.artifacts.by_kind(ArtifactKind::DockerImageV2));
                        arts
                    }
                    other => bail!(
                        "docker_signs[{}]: unknown artifacts filter {:?} (expected one of: \
                         all, images, manifests, none, or empty)",
                        sign_id,
                        other
                    ),
                };

                let pre_ids = docker_artifacts.len();
                let image_paths: Vec<(
                    std::path::PathBuf,
                    std::collections::HashMap<String, String>,
                )> = docker_artifacts
                    .into_iter()
                    .filter(|a| {
                        crate::helpers::sign_ids_match(&a.metadata, docker_sign_cfg.ids.as_ref())
                    })
                    .map(|a| (a.path.clone(), a.metadata.clone()))
                    .collect();

                if anodizer_core::artifact::ids_filter_eliminated_all(
                    docker_sign_cfg.ids.as_deref(),
                    pre_ids,
                    image_paths.len(),
                ) {
                    log.warn(&format!(
                        "ids filter {:?} on docker-sign config '{}' matched no docker \
                         artifacts — this config will sign NOTHING",
                        docker_sign_cfg.ids.as_deref().unwrap_or(&[]),
                        sign_id
                    ));
                }

                for (image_path, metadata) in &image_paths {
                    let image_str = image_path.to_string_lossy();

                    // Set docker-specific template variables from artifact metadata.
                    // `Digest` — the docker image digest (e.g., sha256:abc123...)
                    // `ArtifactID` — the artifact's id field from metadata
                    //
                    // These use PascalCase because Go-style template references like
                    // `{{ .Digest }}` are preprocessed by stripping the leading dot,
                    // resulting in `{{ Digest }}`. The Tera template engine is case-
                    // sensitive, so the variable name must match.
                    //
                    // Always set (even to empty) to avoid stale values from a
                    // previous iteration leaking to this image.
                    let digest_val = metadata.get("digest").map(|s| s.as_str()).unwrap_or("");
                    let artifact_id_val = metadata.get("id").map(|s| s.as_str()).unwrap_or("");
                    ctx.template_vars_mut().set("Digest", digest_val);
                    // Also set lowercase for direct Tera usage ({{ digest }}).
                    ctx.template_vars_mut().set("digest", digest_val);
                    ctx.template_vars_mut().set("ArtifactID", artifact_id_val);
                    // Also set camelCase for direct Tera usage ({{ artifactID }}).
                    ctx.template_vars_mut().set("artifactID", artifact_id_val);

                    // Sign the digest-pinned reference (`<repo>:<tag>@<digest>`),
                    // never the bare tag: a tag can move between build and sign,
                    // so a tag-signature may certify a different image than the
                    // one anodize built (cosign warns and is removing tag
                    // signing). The build stage recorded this image's digest in
                    // metadata; pinning to it certifies exactly that image. When
                    // no digest was captured the reference stays unpinned and we
                    // warn rather than silently sign by tag.
                    if digest_val.is_empty() {
                        log.warn(&format!(
                            "docker-sign [{}]: no digest recorded for image '{}' — \
                             signing by tag, which can certify a moved image. Ensure \
                             the docker build stage captured the image digest.",
                            sign_id, image_str
                        ));
                    }
                    let signed_ref =
                        crate::helpers::pin_image_ref_to_digest(image_str.as_ref(), digest_val);

                    // For Docker images the "signature" concept is embedded;
                    // use a placeholder `.sig` path to satisfy the template
                    // if the user has {{ .Signature }} in their args.
                    let signature_str = format!("{}.sig", signed_ref);

                    let resolved = resolve_sign_args(&args, &signed_ref, &signature_str, None);

                    // Propagate template render errors instead of silently
                    // falling back to the unrendered template string —
                    // passing a literal `{{ Artifact }}` to `cosign sign`
                    // would sign the wrong reference (or fail opaquely).
                    // Sibling `binary-sign` / `sign` path (process.rs) uses
                    // the same `?`-propagation shape.
                    let fully_resolved: Vec<String> = resolved
                        .iter()
                        .map(|arg| {
                            ctx.render_template(arg).with_context(|| {
                                format!("docker-sign [{}]: render arg '{}'", sign_id, arg)
                            })
                        })
                        // `{{ .Artifact }}` already resolves to the pinned ref;
                        // an args template that ALSO appends `@{{ .Digest }}`
                        // (e.g. the historical default) would otherwise yield a
                        // doubled `@sha256:..@sha256:..`. Collapse it so exactly
                        // one digest pin survives regardless of args shape.
                        .map(|arg| arg.map(|a| crate::helpers::collapse_doubled_digest(&a)))
                        .collect::<Result<Vec<_>>>()?;

                    if ctx.is_dry_run() {
                        log.status(&format!(
                            "(dry-run) would run: {} {}",
                            cmd,
                            fully_resolved.join(" ")
                        ));
                        continue;
                    }

                    log.verbose(&format!("docker-sign [{}] {}", sign_id, signed_ref));

                    // Prepare stdin piping for docker signs.
                    let (stdin_cfg, stdin_data) = prepare_stdin_from(
                        docker_sign_cfg.stdin.as_deref(),
                        docker_sign_cfg.stdin_file.as_deref(),
                        "docker-sign",
                    )?;

                    let mut command = Command::new(&cmd);
                    command
                        .args(&fully_resolved)
                        .stdin(stdin_cfg)
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped());

                    // Parse and render docker-sign env in one pass; propagate errors
                    // instead of silently falling back to unrendered template strings.
                    // The rendered pairs are reused for redaction below.
                    let mut docker_rendered_env: Vec<(String, String)> =
                        anodizer_core::config::render_env_entries(
                            docker_sign_cfg.env.as_deref().unwrap_or(&[]),
                            |v| ctx.render_template(v),
                        )
                        .with_context(|| "docker-sign: render env entries")?;

                    // docker image signing is cosign — suppress the sigstore
                    // consent prompt so it never blocks/banners in CI.
                    crate::process::ensure_cosign_consent_env(&cmd, &mut docker_rendered_env);

                    for (k, v) in &docker_rendered_env {
                        command.env(k, v);
                    }

                    let mut child = command.spawn().with_context(|| {
                        format!(
                            "sign: failed to spawn '{}' for docker image {}",
                            cmd, image_str
                        )
                    })?;

                    if let Some(data) = stdin_data {
                        if let Some(mut child_stdin) = child.stdin.take() {
                            child_stdin.write_all(&data).with_context(|| {
                                format!(
                                    "sign: failed to write stdin for docker image {}",
                                    image_str
                                )
                            })?;
                            drop(child_stdin);
                        } else {
                            log.warn(&format!(
                                "stdin data provided but child process stdin unavailable for docker image {}",
                                image_str
                            ));
                        }
                    }

                    let output = child.wait_with_output().with_context(|| {
                        format!(
                            "sign: failed to wait for '{}' for docker image {}",
                            cmd, image_str
                        )
                    })?;

                    // Redact secrets from stdout/stderr before any output or logging.
                    // Use the already-rendered env pairs (rendered values are what
                    // actually appear in command output, so redact those).
                    let docker_env_pairs: Vec<(String, String)> = docker_rendered_env
                        .into_iter()
                        .chain(std::env::vars())
                        .collect();

                    let stdout_raw = String::from_utf8_lossy(&output.stdout).to_string();
                    let stderr_raw = String::from_utf8_lossy(&output.stderr).to_string();

                    let stdout_str = anodizer_core::redact::string(&stdout_raw, &docker_env_pairs);
                    let stderr_str = anodizer_core::redact::string(&stderr_raw, &docker_env_pairs);

                    // Raw subprocess stdio is verbose-only detail (the cosign
                    // tlog lines, sigstore banner): per
                    // .claude/rules/log-status-vs-verbose.md it never belongs at
                    // default. An explicit `output:` template that evaluates true
                    // opts the tee back in; the default is silent (the
                    // `signed image <ref>` RESULT below is the default signal,
                    // and a non-zero exit still surfaces via `check_output`).
                    let show_output = match docker_sign_cfg.output.as_ref() {
                        Some(s) => s
                            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                            .with_context(|| "docker_sign: render output template")?,
                        None => false,
                    };
                    if show_output {
                        if !stdout_str.is_empty() {
                            log.verbose(&format!("[docker-sign stdout] {}", stdout_str.trim()));
                        }
                        if !stderr_str.is_empty() {
                            log.verbose(&format!("[docker-sign stderr] {}", stderr_str.trim()));
                        }
                    }

                    // Redact output bytes before passing to check_output so error
                    // messages from failed docker signing commands don't leak secrets.
                    let mut redacted_output = output;
                    redacted_output.stdout = stdout_str.into_bytes();
                    redacted_output.stderr = stderr_str.into_bytes();

                    // Now check exit status (bails on non-zero).
                    log.check_output(redacted_output, &cmd)?;

                    log.status(&format!("signed image {signed_ref}")); // status-ok: per-image sign result
                }
            }

            // Clear docker-specific template vars so they don't leak to
            // downstream stages that may inspect the template context.
            ctx.template_vars_mut().set("Digest", "");
            ctx.template_vars_mut().set("digest", "");
            ctx.template_vars_mut().set("ArtifactID", "");
            ctx.template_vars_mut().set("artifactID", "");
        }

        // Refresh the artifacts template variable so newly-added signatures
        // and certificates are visible to downstream stages (matching
        // refresh the artifact registry).
        ctx.refresh_artifacts_var();

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;

/// Requirements shared by one active sign-config entry: the signing command
/// itself plus every env var its templated args/env/stdin reference. cosign
/// `env://VAR` key refs are declared as loadable cosign key material; for any
/// other command only presence of the referenced vars can be required.
fn entry_env_requirements(
    cmd: &str,
    strings: impl Iterator<Item = String>,
    out: &mut Vec<anodizer_core::EnvRequirement>,
) {
    use anodizer_core::env_preflight::{env_scheme_refs, template_env_refs};
    out.push(anodizer_core::EnvRequirement::Tool {
        name: cmd.to_string(),
    });
    let is_cosign = crate::process::is_cosign_cmd(cmd);
    for s in strings {
        let refs = template_env_refs(&s);
        if !refs.is_empty() {
            out.push(anodizer_core::EnvRequirement::EnvAllOf { vars: refs });
        }
        for var in env_scheme_refs(&s) {
            if is_cosign {
                out.push(anodizer_core::EnvRequirement::KeyEnv {
                    kind: anodizer_core::KeyKind::Cosign,
                    var,
                });
            } else {
                out.push(anodizer_core::EnvRequirement::EnvAllOf { vars: vec![var] });
            }
        }
    }
}

/// Requirements for a `signs:` / `binary_signs:` slice given its
/// `artifacts` fallback (`"none"` for `signs`, `"binary"` for
/// `binary_signs`) — entries whose resolved filter is `"none"` are inert
/// and declare nothing.
fn sign_slice_requirements(
    configs: &[anodizer_core::config::SignConfig],
    fallback_artifacts: &str,
) -> Vec<anodizer_core::EnvRequirement> {
    let mut out = Vec::new();
    for cfg in configs {
        // An `authenticode:` config has its own derived signer + cert lifecycle
        // and never invokes the cosign/gpg cmd, so its requirements are derived
        // separately. Preflight must match RUNTIME exactly:
        //
        // - When NOT required, the runtime skips gracefully if no cert resolves
        //   (`process_authenticode_config`), so the config may legitimately run
        //   nothing — declare NOTHING, including the tool (requiring an absent
        //   osslsigncode/signtool would falsely fail a permitted skip).
        // - When required, the runtime hard-fails on a missing cert AND a
        //   missing tool, so both become preflight requirements. But the cert
        //   env var is only the cert source when `cert_file` is unset; a
        //   literal/templated `cert_file` supplies the cert directly, so
        //   requiring the env var then would be a false positive.
        // - The password is OPTIONAL at runtime even when required (a
        //   passwordless .p12 is valid), so it is never a preflight requirement.
        if let Some(authenticode) = &cfg.authenticode {
            if authenticode.is_required() {
                out.push(anodizer_core::EnvRequirement::Tool {
                    name: authenticode.resolved_tool().to_string(),
                });
                if authenticode.cert_file.is_none() {
                    out.push(anodizer_core::EnvRequirement::EnvAllOf {
                        vars: vec![authenticode.resolved_cert_env().to_string()],
                    });
                }
            }
            continue;
        }
        if cfg.resolved_artifacts(fallback_artifacts) == "none" {
            continue;
        }
        let cmd = cfg.cmd.clone().unwrap_or_else(default_sign_cmd);
        let strings = cfg
            .args
            .iter()
            .flatten()
            .chain(cfg.env.iter().flatten())
            .chain(cfg.stdin.iter())
            .chain(cfg.certificate.iter())
            .cloned();
        entry_env_requirements(&cmd, strings, &mut out);
    }
    out
}

/// Environment requirements for the sign stage (`signs:` entries).
pub fn sign_env_requirements(
    ctx: &anodizer_core::context::Context,
) -> Vec<anodizer_core::EnvRequirement> {
    sign_slice_requirements(
        &ctx.config.signs,
        anodizer_core::config::SignConfig::DEFAULT_ARTIFACTS,
    )
}

/// Environment requirements for the binary-sign stage (`binary_signs:`).
pub fn binary_sign_env_requirements(
    ctx: &anodizer_core::context::Context,
) -> Vec<anodizer_core::EnvRequirement> {
    sign_slice_requirements(
        &ctx.config.binary_signs,
        anodizer_core::config::SignConfig::DEFAULT_ARTIFACTS_BINARY,
    )
}

/// Environment requirements for the docker-sign stage (`docker_signs:`):
/// active only when some crate builds docker images, since the stage no-ops
/// without `DockerImageV2` artifacts.
pub fn docker_sign_env_requirements(
    ctx: &anodizer_core::context::Context,
) -> Vec<anodizer_core::EnvRequirement> {
    let any_images = ctx
        .config
        .crate_universe()
        .into_iter()
        .flat_map(|c| c.dockers_v2.iter().flatten())
        .any(|d| {
            !d.skip.as_ref().is_some_and(|v| {
                v.try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                    .unwrap_or(false)
            })
        });
    if !any_images {
        return Vec::new();
    }
    let mut out = Vec::new();
    for cfg in ctx.config.docker_signs.iter().flatten() {
        if cfg.resolved_artifacts() == "none" {
            continue;
        }
        let strings = cfg
            .resolved_args()
            .into_iter()
            .chain(cfg.env.iter().flatten().cloned())
            .chain(cfg.stdin.clone());
        entry_env_requirements(cfg.resolved_cmd(), strings, &mut out);
    }
    out
}
