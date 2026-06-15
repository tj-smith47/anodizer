//! Template-rendering helpers — `render_url_template` for `url_template`
//! strings (winget/scoop/krew) and `render_or_warn` for non-strict template
//! evaluation with a logged warning on failure.

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::template::{self, TemplateVars, assert_no_unrendered};
use anyhow::{Result, bail};

/// Render a `url_template` string with Tera, providing only the four
/// per-artifact lower-case helper vars: `name`, `version`, `arch`, `os`.
///
/// Prefer [`render_url_template_with_ctx`] for new call sites — that variant
/// also exposes the full project template surface (`ProjectName`, `Tag`,
/// `Version`, `Env.*`, `ArtifactName`, etc.) so a dotted-variable config
/// like `url_template: "{{ .Tag }}/{{ .ArtifactName }}"` resolves correctly.
/// This thin wrapper is retained for the rare site that has no `&Context`
/// available (and only for backward compatibility).
pub(crate) fn render_url_template(
    url_template: &str,
    name: &str,
    version: &str,
    arch: &str,
    os: &str,
) -> String {
    let mut vars = TemplateVars::new();
    vars.set("name", name);
    vars.set("version", version);
    vars.set("arch", arch);
    vars.set("os", os);
    template::render(url_template, &vars).unwrap_or_else(|_| url_template.to_string())
}

/// Render a `url_template` string with the full context template-vars surface
/// (Tag, ProjectName, Version, Env.\*, Major/Minor/Patch, Commit, Branch,
/// PreviousTag, ArtifactName, …) plus the per-artifact overlays
/// (`name`, `version`, `arch`, `os`, `Os`, `Arch`, `Binary`, `ArtifactName`).
///
/// The url-template render exposes
/// 30+ variables to publisher URL templates; without overlay-style merging,
/// migrated configs that reference `{{ .Tag }}` or `{{ .Env.GITHUB_TOKEN }}`
/// silently produce empty fields.
///
/// On render error (malformed template), returns the raw input unchanged —
/// matching the legacy [`render_url_template`] failure path.
pub(crate) fn render_url_template_with_ctx(
    ctx: &Context,
    url_template: &str,
    name: &str,
    version: &str,
    arch: &str,
    os: &str,
) -> String {
    render_url_template_with_ctx_and_artifact(ctx, url_template, name, None, version, arch, os)
}

/// Like [`render_url_template_with_ctx`] but also sets `ArtifactName`
/// unconditionally from an explicit artifact filename.
///
/// Use this variant when the caller has a project/crate `name` (no extension)
/// AND a separate `artifact_name` (the archive filename, e.g.
/// `tool-1.2.0-linux-amd64.tar.gz`). The `name` project token and the
/// `ArtifactName` archive filename are then independently available in the
/// template.
pub(crate) fn render_url_template_with_ctx_and_artifact(
    ctx: &Context,
    url_template: &str,
    name: &str,
    artifact_name: Option<&str>,
    version: &str,
    arch: &str,
    os: &str,
) -> String {
    // Start from the full project template-vars surface and overlay the
    // per-artifact pieces. The clone is cheap (small string maps) and keeps
    // the original `ctx.template_vars()` immutable for sibling calls.
    let mut vars = ctx.template_vars().clone();
    // Per-artifact overlays — both the lower-case shorthand (legacy
    // `name`/`version`/`arch`/`os`) and the canonical `Os`/`Arch`
    // dotted keys, so a config of either flavor renders.
    vars.set("name", name);
    vars.set("version", version);
    vars.set("arch", arch);
    vars.set("os", os);
    vars.set("Os", os);
    vars.set("Arch", arch);
    match artifact_name {
        // Explicit artifact filename takes precedence.
        Some(af) => vars.set("ArtifactName", af),
        // When no explicit artifact filename is given, fall back: set
        // ArtifactName only if `name` itself looks like a filename (has an
        // extension). This preserves callers that pass a project/cask token.
        None => {
            if name.contains('.') {
                vars.set("ArtifactName", name);
            }
        }
    }
    template::render(url_template, &vars).unwrap_or_else(|_| url_template.to_string())
}

/// Render `raw` through the context template engine, named by `field` (e.g.
/// `"aur.name"`, `"winget.description"`, `"scoop.name"`).
///
/// Behaviour depends on [`Context::render_is_strict`]:
/// - **Strict** (the pre-publish guard's render pass, or the user's global
///   `--strict`): a malformed template returns `Err`, naming the `field`, so a
///   broken publisher template fails the release loud BEFORE any irreversible
///   publisher fires.
/// - **Lenient** (production dry-run / snapshot / nightly publish): a malformed
///   template logs a `log.warn` describing the failed `field` and falls back to
///   the raw value, keeping a currently-malformed config building while making
///   the error visible in stage output.
///
/// Earlier these sites used `ctx.render_template(...).unwrap_or_else(|_|
/// raw.clone())`, which silently swallowed malformed-template errors and
/// propagated the raw string downstream — defeating both debuggability and the
/// guard. Routing every site through here closes that gap.
///
/// `field` should carry the namespace (e.g. `"aur.name"`,
/// `"aur_source.directory"`); the warn message does not prepend a stage
/// prefix because `StageLogger` already does that for every line.
pub(crate) fn render_or_warn(
    ctx: &Context,
    log: &StageLogger,
    field: &str,
    raw: &str,
) -> Result<String> {
    render_or_warn_with_vars(ctx.template_vars(), log, field, raw, ctx.render_is_strict())
}

/// Like [`render_or_warn`], but renders against an explicit `vars` set instead
/// of the context's global template vars, and takes `is_strict` directly
/// (callers pass [`Context::render_is_strict`]) since there is no `&Context`
/// in hand.
///
/// Used where a publisher scopes an extra template variable for a single
/// resource's renders (e.g. the AUR-source `Amd64` micro-architecture variable)
/// and the `directory:` / `url_template:` strings must see that same scoped
/// value — rendering them against the global vars would resolve the scoped
/// variable to its stale/empty global value.
pub(crate) fn render_or_warn_with_vars(
    vars: &TemplateVars,
    log: &StageLogger,
    field: &str,
    raw: &str,
    is_strict: bool,
) -> Result<String> {
    match template::render(raw, vars) {
        Ok(rendered) => Ok(rendered),
        Err(e) => {
            if is_strict {
                bail!("failed to render {field} template {raw:?}: {e}");
            }
            log.warn(&format!(
                "failed to render {field} template {raw:?}: {e}; \
                 falling back to raw value"
            ));
            Ok(raw.to_string())
        }
    }
}

/// Final-text guard: after a publisher renders its manifest to the finished
/// `text`, assert it carries no residual Go/Tera `{{ … }}` delimiters — a
/// residual means a user-supplied config string field was emitted without
/// being template-rendered (the bug class this guard makes unrepresentable).
///
/// `label` names the publisher + manifest (e.g. `"chocolatey nuspec"`). The
/// offending snippet is redacted via [`Context::redact`] before it reaches any
/// log line or error, so a secret-flagged value never leaks.
///
/// - **strict** ([`Context::render_is_strict`]): returns `Err`, failing the
///   publish before any irreversible step.
/// - **non-strict**: `log.warn`s the redacted snippet and returns `Ok(())`.
///
/// Intended for manifest formats that never legitimately contain `{{ }}`
/// (nuspec/JSON/YAML/Ruby/nix/PKGBUILD); do NOT apply to verbatim/raw paths
/// (e.g. announce `--raw`) where unrendered delimiters are by design.
pub(crate) fn guard_no_unrendered(
    ctx: &Context,
    log: &StageLogger,
    label: &str,
    text: &str,
) -> Result<()> {
    let residual = assert_no_unrendered(text, label, ctx.render_is_strict(), |s| ctx.redact(s))?;
    if let Some(r) = residual {
        log.warn(&format!(
            "{label}: unrendered template delimiter in generated manifest: {:?} \
             (a user-supplied config field was emitted without template rendering)",
            r.snippet
        ));
    }
    Ok(())
}
