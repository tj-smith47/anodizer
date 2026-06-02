// Template rendering powered by Tera.
// Supports both Go-style `{{ .Field }}` and Tera-style `{{ Field }}`.
// Go-style templates are preprocessed (leading dots stripped) before Tera renders them.
// Tera gives us: if/else/endif, for loops, pipes (| lower, | upper, | replace),
// | default, | trim, | title, and many more built-in filters.
//
// ## Template-render-error handling policy
//
// **Hard-bail on any user-supplied template render error.**
//
// Silently falling back to the raw template string (or to an empty value) on a
// render failure has burned users repeatedly: a typo like `{{ .Teg }}` in a
// signature path is rendered-as-literal and the signer is invoked with a path
// that doesn't exist, producing a confusing downstream error rather than a
// clear template diagnostic.
//
// Callers that render user-supplied templates (config fields, hook commands,
// signing args, release header/footer, etc.) MUST propagate the
// `render_template` / `render` error via `?` or `.with_context(...)?`. If you
// see `.unwrap_or_else(|e| { log.warn(...); raw.clone() })` in lib code,
// convert it to a bail with a stage-named context message.
//
// The single exception is `render_template_opt` for purely internal defaults
// (e.g. "default icon url") where a render-failure genuinely has a sensible
// fallback — those sites should be rare and commented.
//
// Enforcement: code review + the `anti-patterns.md` hook (which already
// forbids `.unwrap()` in lib code, catching most drift automatically).

mod base_tera;
mod render;
mod static_render;
mod vars;

#[cfg(test)]
mod tests;

pub use base_tera::ruby_escape_str;
pub use render::{extract_artifact_ext, render};
pub use static_render::{parse_static, render_static};
pub use vars::{
    PER_ARTIFACT_VARS, PER_TARGET_VARS, TemplateVars, clear_per_artifact_vars,
    clear_per_target_vars,
};
