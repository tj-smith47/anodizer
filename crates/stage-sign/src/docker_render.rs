//! Per-image rendering for `docker_signs`: every image's argv and child env
//! are rendered before the first signature is made, so each decision about
//! the signer (the harness skip, the verification mode, the host TUF lock)
//! is taken on the values the spawn will use.

use anyhow::{Context as _, Result};

use anodizer_core::config::DockerSignConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;

use crate::helpers::{collapse_doubled_digest, pin_image_ref_to_digest, resolve_sign_args};
use crate::process::{
    ensure_cosign_consent_env, harden_cosign_args_for_harness, is_keyless_cosign_under_harness,
    render_args_without_artifact,
};

/// A docker image selected for signing: its reference and build metadata.
pub(crate) type ImageEntry = (
    std::path::PathBuf,
    std::collections::HashMap<String, String>,
);

/// One image's fully rendered sign invocation.
pub(crate) struct RenderedImageSign {
    /// The digest-pinned reference the signature certifies.
    pub(crate) signed_ref: String,
    /// The argv the spawn uses, rendered and harness-hardened.
    pub(crate) argv: Vec<String>,
    /// The child `env:` overlay the image's TUF store is locked from.
    pub(crate) env: Vec<(String, String)>,
}

/// Seed the docker-image template variables from one image's build metadata
/// and return that image's digest — empty when the build stage captured none.
///
/// `Digest` / `ArtifactID` are PascalCase because a Go-style `{{ .Digest }}`
/// reference is preprocessed to `{{ Digest }}` and Tera is case-sensitive; the
/// `digest` / `artifactID` spellings serve templates written against Tera
/// directly. All four are set on every image, even to empty, so no value
/// leaks from the previously rendered one.
pub(crate) fn set_image_template_vars<'a>(
    ctx: &mut Context,
    metadata: &'a std::collections::HashMap<String, String>,
) -> &'a str {
    let digest = metadata.get("digest").map(|s| s.as_str()).unwrap_or("");
    let artifact_id = metadata.get("id").map(|s| s.as_str()).unwrap_or("");
    ctx.template_vars_mut().set("Digest", digest);
    ctx.template_vars_mut().set("digest", digest);
    ctx.template_vars_mut().set("ArtifactID", artifact_id);
    ctx.template_vars_mut().set("artifactID", artifact_id);
    digest
}

/// Render every selected image's sign invocation up front, once per image.
///
/// The spawn loop reuses each argv and env verbatim, and every decision
/// about the signer — the harness skip, the verification mode, the host TUF
/// lock — is taken on these rendered values, never on the config's template
/// strings: a `--key` can arrive through a template, and a `TUF_ROOT` that
/// depends on the image (`{{ .Digest }}`, `{{ .ArtifactID }}`) names a store
/// that must be known — and locked — before the first signature is made. A
/// dry run spawns nothing and renders no env, matching the loop, which
/// prints the argv and moves on.
///
/// Returns `None` when the determinism harness skips the config: keyless
/// cosign cannot run there (no ambient OIDC; the ephemeral `COSIGN_KEY` env
/// crashes a `--key`-less invocation), and one keyless image skips the whole
/// config. A config that matched no image renders no per-image argv, so it
/// is classified once from the config-level render instead. A
/// `--key`-bearing config still runs.
pub(crate) fn render_image_signs(
    ctx: &mut Context,
    log: &StageLogger,
    cfg: &DockerSignConfig,
    sign_id: &str,
    cmd: &str,
    args: &[String],
    image_paths: &[ImageEntry],
) -> Result<Option<Vec<RenderedImageSign>>> {
    if image_paths.is_empty()
        && is_keyless_cosign_under_harness(cmd, &render_args_without_artifact(args, ctx), ctx)
    {
        return Ok(None);
    }

    let mut per_image: Vec<RenderedImageSign> = Vec::with_capacity(image_paths.len());
    for (image_path, metadata) in image_paths {
        let image_str = image_path.to_string_lossy();
        let digest_val = set_image_template_vars(ctx, metadata);

        // Sign the digest-pinned reference (`<repo>:<tag>@<digest>`), never
        // the bare tag: a tag can move between build and sign, so a
        // tag-signature may certify a different image than the one anodizer
        // built (cosign warns and is removing tag signing). The build stage
        // recorded this image's digest in metadata; pinning to it certifies
        // exactly that image. When no digest was captured the reference
        // stays unpinned and the operator is warned rather than silently
        // signing by tag.
        if digest_val.is_empty() {
            log.warn(&format!(
                "docker-sign [{}]: no digest recorded for image '{}' — \
                 signing by tag, which can certify a moved image. Ensure \
                 the docker build stage captured the image digest.",
                sign_id, image_str
            ));
        }
        let signed_ref = pin_image_ref_to_digest(image_str.as_ref(), digest_val);

        // For Docker images the "signature" concept is embedded; a
        // placeholder `.sig` path satisfies an args template that names
        // `{{ .Signature }}`.
        let signature_str = format!("{}.sig", signed_ref);

        let resolved = resolve_sign_args(args, &signed_ref, &signature_str, None);

        // A render error propagates instead of falling back to the template
        // string: a literal `{{ Artifact }}` handed to `cosign sign` would
        // sign the wrong reference (or fail opaquely).
        let argv: Vec<String> = resolved
            .iter()
            .map(|arg| {
                ctx.render_template(arg)
                    .with_context(|| format!("docker-sign [{}]: render arg '{}'", sign_id, arg))
            })
            // `{{ .Artifact }}` already resolves to the pinned ref; an args
            // template that ALSO appends `@{{ .Digest }}` (the historical
            // default) would otherwise yield a doubled `@sha256:..@sha256:..`.
            // Collapse it so exactly one digest pin survives.
            .map(|arg| arg.map(|a| collapse_doubled_digest(&a)))
            .collect::<Result<Vec<_>>>()?;
        let argv = harden_cosign_args_for_harness(cmd, argv, ctx);

        if is_keyless_cosign_under_harness(cmd, &argv, ctx) {
            return Ok(None);
        }

        let env = if ctx.is_dry_run() {
            Vec::new()
        } else {
            let mut env =
                anodizer_core::config::render_env_entries(cfg.env.as_deref().unwrap_or(&[]), |v| {
                    ctx.render_template(v)
                })
                .with_context(|| "docker-sign: render env entries")?;
            // Docker image signing is cosign — suppress the sigstore consent
            // prompt so it never blocks or banners in CI.
            ensure_cosign_consent_env(cmd, &mut env);
            env
        };
        per_image.push(RenderedImageSign {
            signed_ref,
            argv,
            env,
        });
    }
    Ok(Some(per_image))
}
