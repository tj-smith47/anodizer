//! Template-rendering helpers — `render_url_template` for `url_template`
//! strings (winget/scoop/krew) and `render_or_warn` for non-strict template
//! evaluation with a logged warning on failure.

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::template::{self, TemplateVars};

/// Render a `url_template` string with Tera, providing only the four
/// per-artifact lower-case helper vars: `name`, `version`, `arch`, `os`.
///
/// Prefer [`render_url_template_with_ctx`] for new call sites — that variant
/// also exposes the full project template surface (`ProjectName`, `Tag`,
/// `Version`, `Env.*`, `ArtifactName`, etc.) so a migrated GoReleaser config
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
/// GoReleaser's `tmpl.New(ctx).WithArtifact(art).Apply(url_template)` exposes
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
    // Start from the full project template-vars surface and overlay the
    // per-artifact pieces. The clone is cheap (small string maps) and keeps
    // the original `ctx.template_vars()` immutable for sibling calls.
    let mut vars = ctx.template_vars().clone();
    // Per-artifact overlays — both the lower-case shorthand (legacy
    // `name`/`version`/`arch`/`os`) and the canonical `Os`/`Arch`
    // GR keys, so a config of either flavor renders.
    vars.set("name", name);
    vars.set("version", version);
    vars.set("arch", arch);
    vars.set("os", os);
    vars.set("Os", os);
    vars.set("Arch", arch);
    // Only set ArtifactName if the caller's `name` looks like an artifact
    // filename (has an extension). Otherwise leave whatever the context
    // already populated. This preserves callers that pass a project/cask
    // token rather than an artifact filename.
    if name.contains('.') {
        vars.set("ArtifactName", name);
    }
    template::render(url_template, &vars).unwrap_or_else(|_| url_template.to_string())
}

/// Render `raw` through the context template engine and on error emit a
/// `log.warn` describing the failed `field` (e.g. `"aur.name"`,
/// `"aur.description"`, `"aur.directory"`) and fall back to the raw value.
///
/// Originally these sites used `ctx.render_template(...).unwrap_or_else(|_|
/// raw.clone())` which silently swallowed malformed-template errors and
/// propagated the raw string downstream — defeating debuggability. The
/// non-strict warn-and-fallback path keeps currently-malformed user
/// configs building (no behavior regression) while making the error
/// visible in stage output.
///
/// `field` should carry the namespace (e.g. `"aur.name"`,
/// `"aur_source.directory"`); the warn message does not prepend a stage
/// prefix because `StageLogger` already does that for every line.
pub(crate) fn render_or_warn(ctx: &Context, log: &StageLogger, field: &str, raw: &str) -> String {
    match ctx.render_template(raw) {
        Ok(rendered) => rendered,
        Err(e) => {
            log.warn(&format!(
                "failed to render {field} template {raw:?}: {e}; \
                 falling back to raw value"
            ));
            raw.to_string()
        }
    }
}
