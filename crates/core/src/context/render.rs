use super::*;
use anyhow::Context as _;

impl Context {
    pub fn template_vars(&self) -> &TemplateVars {
        &self.template_vars
    }

    pub fn template_vars_mut(&mut self) -> &mut TemplateVars {
        &mut self.template_vars
    }

    pub fn render_template(&self, template: &str) -> anyhow::Result<String> {
        crate::template::render(template, &self.template_vars)
    }

    /// Render `template` with the FULL version-derived var set (`Version`, `Tag`,
    /// `Major`/`Minor`/`Patch`, `RawVersion`, `Prerelease`, `BuildMetadata`,
    /// `Base`) re-derived from `version`/`tag` rather than the context's own git
    /// version — used by promotion to reconstruct the immutable tag a prior
    /// release pushed for a specific `--version`. Overriding only `Version`+`Tag`
    /// would leave `{{ .Major }}` etc. resolving to the CONTEXT version, so a
    /// `{{ .Major }}.{{ .Minor }}.{{ .Patch }}` tag template would render the
    /// wrong source tag whenever the context version differs from the target.
    ///
    /// A non-semver `version` (no parse) falls back to overriding `Version`+`Tag`
    /// and BLANKING the seven semver-part vars (`RawVersion`, `Base`, `Major`,
    /// `Minor`, `Patch`, `Prerelease`, `BuildMetadata`), so a semver-part template
    /// cannot silently resolve the context version — it renders empty instead of
    /// inheriting the cloned context's parts. Does not mutate `self`.
    pub fn render_template_for_version(
        &self,
        template: &str,
        version: &str,
        tag: &str,
    ) -> anyhow::Result<String> {
        let mut vars = self.template_vars.clone();
        match crate::git::parse_semver(version) {
            Ok(semver) => set_version_vars(&mut vars, &semver, tag),
            Err(_) => {
                vars.set("Version", version);
                vars.set("Tag", tag);
                // Blank the semver-derived vars `set_version_vars` writes so a
                // `{{ .Major }}`-style template renders empty rather than
                // inheriting the cloned context version's parts (a false match).
                for key in [
                    "RawVersion",
                    "Base",
                    "Major",
                    "Minor",
                    "Patch",
                    "Prerelease",
                    "BuildMetadata",
                ] {
                    vars.set(key, "");
                }
            }
        }
        crate::template::render(template, &vars)
    }

    /// Render a template if present, returning `None` for `None` input.
    pub fn render_template_opt(&self, template: Option<&str>) -> anyhow::Result<Option<String>> {
        template.map(|t| self.render_template(t)).transpose()
    }

    /// Evaluate a `skip` field, logging at INFO level when it resolves to true.
    ///
    /// Returns `Ok(false)` when `skip` is `None` or evaluates falsy. On
    /// truthy, writes `"{label} skipped"` via `log.status` and returns
    /// `Ok(true)`. A malformed `skip:` template propagates as `Err` so the
    /// caller fails fast — silently treating a render error as "not skipped"
    /// (the prior behavior) shipped configs that the user thought would
    /// suppress a stage but actually ran it.
    pub fn skip_with_log(
        &self,
        skip: &Option<crate::config::StringOrBool>,
        log: &StageLogger,
        label: &str,
    ) -> anyhow::Result<bool> {
        let Some(d) = skip else {
            return Ok(false);
        };
        let should_skip = d
            .try_evaluates_to_true(|s| self.render_template(s))
            .with_context(|| format!("evaluate skip expression for {label}"))?;
        if should_skip {
            log.status(&format!("{} skipped", label));
        }
        Ok(should_skip)
    }

    /// Render a template, failing in strict mode on error, or falling back to the raw string.
    pub fn render_template_strict(
        &self,
        template: &str,
        label: &str,
        log: &crate::log::StageLogger,
    ) -> anyhow::Result<String> {
        match self.render_template(template) {
            Ok(rendered) => Ok(rendered),
            Err(e) => {
                if self.options.strict {
                    anyhow::bail!("{}: failed to render template: {} (strict mode)", label, e);
                }
                log.warn(&format!("failed to render template for {}: {}", label, e));
                Ok(template.to_string())
            }
        }
    }
}
