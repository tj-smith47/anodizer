use anyhow::{Context as _, Result};

/// Parse a static (compile-time) Tera template, returning a `tera::Tera`
/// instance with the template registered under `name`.
///
/// Use this for "trusted" templates baked into the binary (PKGBUILD body,
/// cask/formula skeletons, nuspec, etc.) where parse failure is a programmer
/// bug, but we still want the error to flow through `Result` rather than a
/// panic site so the anti-pattern hook stays clean and the caller's stage
/// label reaches the user as `.with_context(...)?`.
pub fn parse_static(name: &str, raw: &str) -> Result<tera::Tera> {
    let mut tera = tera::Tera::default();
    // Empty suffix list = escape nothing; the element type must be named for
    // 2.0's generic `IntoIterator` signature to infer.
    tera.autoescape_on(Vec::<&str>::new());
    super::base_tera::register_ruby_escape(&mut tera);
    // Same raw-string-literal restoration the dynamic render path applies:
    // the embedded templates were authored against 1.x's raw literals.
    let raw = super::engine_adapter::double_string_literal_backslashes(raw);
    tera.add_raw_template(name, raw.as_ref())
        .with_context(|| format!("parse static template '{}'", name))?;
    Ok(tera)
}

/// Render a previously-registered Tera template with `ctx`, returning the
/// rendered string. Stage label is included in the error context so a render
/// failure surfaces as `<stage>: render '<name>': <tera-msg>` rather than a
/// panic.
pub fn render_static(
    tera: &tera::Tera,
    name: &str,
    ctx: &tera::Context,
    stage: &str,
) -> Result<String> {
    tera.render(name, ctx)
        .with_context(|| format!("{}: render '{}'", stage, name))
}
