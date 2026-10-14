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

    /// [`render_template`](Self::render_template) with `pairs` overlaid as
    /// env for this one render. The closure shape
    /// [`crate::config::render_env_entries`] asks for.
    pub fn render_template_with_env(
        &self,
        template: &str,
        pairs: &[(String, String)],
    ) -> anyhow::Result<String> {
        crate::template::render(template, &self.template_vars.with_env_overlay(pairs))
    }

    /// Render `template` with the FULL version-derived var set (`Version`, `Tag`,
    /// `Major`/`Minor`/`Patch`, `RawVersion`, `Prerelease`, `BuildMetadata`,
    /// `Base`) re-derived from `version`/`tag`, ignoring the context's own git
    /// version — used by promotion to reconstruct the immutable tag a prior
    /// release pushed for a specific `--version`. Overriding only `Version`+`Tag`
    /// would leave `{{ .Major }}` etc. resolving to the CONTEXT version, so a
    /// `{{ .Major }}.{{ .Minor }}.{{ .Patch }}` tag template would render the
    /// wrong source tag whenever the context version differs from the target.
    ///
    /// A non-semver `version` (no parse) falls back to overriding `Version`+`Tag`
    /// and BLANKING the seven semver-part vars (`RawVersion`, `Base`, `Major`,
    /// `Minor`, `Patch`, `Prerelease`, `BuildMetadata`), so a semver-part template
    /// cannot silently resolve the context version — it renders empty and
    /// never inherits the cloned context's parts. Does not mutate `self`.
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
                // `{{ .Major }}`-style template renders empty; inheriting the
                // cloned context version's parts would be a false match.
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

    /// Render the credential template `raw` of the config field `field` and
    /// return it trimmed; a template that fails to render is an error in every
    /// mode, naming `field` without the template's text
    /// ([`crate::git::render_secret`]).
    pub fn render_secret(&self, field: &str, raw: &str) -> anyhow::Result<String> {
        crate::git::render_secret(field, raw, &|s| self.render_template(s))
    }

    /// [`render_secret`](Self::render_secret) of an optional field, `None`
    /// for `None` input.
    pub fn render_secret_opt(
        &self,
        field: &str,
        raw: Option<&str>,
    ) -> anyhow::Result<Option<String>> {
        raw.map(|r| self.render_secret(field, r)).transpose()
    }

    /// A credential from the config field `field`, rendered through
    /// [`render_secret`](Self::render_secret), or, when that is unset or
    /// renders blank, from the first of the variables `env_vars` that holds a
    /// value. Each value is trimmed, a blank variable counts as unset
    /// ([`crate::git::non_blank_token`]), and the result is `None` when no
    /// source has a value.
    pub fn config_or_env_secret(
        &self,
        field: &str,
        configured: Option<&str>,
        env_vars: &[&str],
    ) -> anyhow::Result<Option<String>> {
        let configured = self
            .render_secret_opt(field, configured)?
            .filter(|v| !v.is_empty());
        Ok(configured.or_else(|| {
            env_vars.iter().find_map(|name| {
                self.env_var(name)
                    .as_deref()
                    .and_then(crate::git::non_blank_token)
            })
        }))
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
