use crate::artifact::ArtifactRegistry;
use crate::config::Config;
use crate::git::GitInfo;
use crate::template::TemplateVars;

#[derive(Default)]
pub struct ContextOptions {
    pub snapshot: bool,
    pub dry_run: bool,
    pub skip_stages: Vec<String>,
    pub selected_crates: Vec<String>,
    pub token: Option<String>,
}

pub struct Context {
    pub config: Config,
    pub artifacts: ArtifactRegistry,
    pub options: ContextOptions,
    template_vars: TemplateVars,
    pub git_info: Option<GitInfo>,
}

impl Context {
    pub fn new(config: Config, options: ContextOptions) -> Self {
        let mut vars = TemplateVars::new();
        vars.set("ProjectName", &config.project_name);
        Self {
            config,
            artifacts: ArtifactRegistry::new(),
            options,
            template_vars: vars,
            git_info: None,
        }
    }

    pub fn template_vars(&self) -> &TemplateVars {
        &self.template_vars
    }

    pub fn template_vars_mut(&mut self) -> &mut TemplateVars {
        &mut self.template_vars
    }

    pub fn render_template(&self, template: &str) -> anyhow::Result<String> {
        crate::template::render(template, &self.template_vars)
    }

    pub fn should_skip(&self, stage_name: &str) -> bool {
        self.options.skip_stages.iter().any(|s| s == stage_name)
    }

    pub fn is_dry_run(&self) -> bool {
        self.options.dry_run
    }

    pub fn is_snapshot(&self) -> bool {
        self.options.snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn test_context_template_vars() {
        let mut config = Config::default();
        config.project_name = "test-project".to_string();
        let ctx = Context::new(config, ContextOptions::default());
        assert_eq!(ctx.template_vars().get("ProjectName"), Some(&"test-project".to_string()));
    }

    #[test]
    fn test_context_should_skip() {
        let config = Config::default();
        let opts = ContextOptions {
            skip_stages: vec!["publish".to_string(), "announce".to_string()],
            ..Default::default()
        };
        let ctx = Context::new(config, opts);
        assert!(ctx.should_skip("publish"));
        assert!(ctx.should_skip("announce"));
        assert!(!ctx.should_skip("build"));
    }

    #[test]
    fn test_context_render_template() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        let ctx = Context::new(config, ContextOptions::default());
        let result = ctx.render_template("{{ .ProjectName }}-release").unwrap();
        assert_eq!(result, "myapp-release");
    }
}
