//! Template-rendering helpers — `render_url_template` for `url_template`
//! strings (winget/scoop/krew) and `render_or_warn` for non-strict template
//! evaluation with a logged warning on failure.

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::template::{self, TemplateVars};

/// Render a `url_template` string with Tera, providing `name`, `version`,
/// `arch`, and `os` variables.  Returns the rendered URL.
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
