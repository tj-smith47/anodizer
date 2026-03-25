// Template rendering — Go-style {{ .Field }} engine.

use std::collections::HashMap;
use anyhow::Result;
use regex::Regex;

pub struct TemplateVars {
    vars: HashMap<String, String>,
    env: HashMap<String, String>,
}

impl TemplateVars {
    pub fn new() -> Self {
        Self { vars: HashMap::new(), env: HashMap::new() }
    }

    pub fn set(&mut self, key: &str, value: &str) {
        self.vars.insert(key.to_string(), value.to_string());
    }

    pub fn get(&self, key: &str) -> Option<&String> {
        self.vars.get(key)
    }

    pub fn set_env(&mut self, key: &str, value: &str) {
        self.env.insert(key.to_string(), value.to_string());
    }
}

impl Default for TemplateVars {
    fn default() -> Self {
        Self::new()
    }
}

pub fn render(template: &str, vars: &TemplateVars) -> Result<String> {
    let re = Regex::new(r"\{\{\s*\.(\w+(?:\.\w+)*)\s*\}\}")?;
    let mut result = template.to_string();
    let matches: Vec<(String, String)> = re
        .captures_iter(template)
        .map(|cap| (cap[0].to_string(), cap[1].to_string()))
        .collect();
    for (full_match, key) in matches {
        let value = if let Some(env_key) = key.strip_prefix("Env.") {
            vars.env
                .get(env_key)
                .ok_or_else(|| anyhow::anyhow!("unknown env variable: {}", env_key))?
        } else {
            vars.vars
                .get(&key)
                .ok_or_else(|| anyhow::anyhow!("unknown template variable: {}", key))?
        };
        result = result.replace(&full_match, value);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_vars() -> TemplateVars {
        let mut vars = TemplateVars::new();
        vars.set("ProjectName", "cfgd");
        vars.set("Version", "1.2.3");
        vars.set("Tag", "v1.2.3");
        vars.set("Os", "linux");
        vars.set("Arch", "amd64");
        vars.set("ShortCommit", "abc1234");
        vars.set("Major", "1");
        vars.set("Minor", "2");
        vars.set("Patch", "3");
        vars.set_env("GITHUB_TOKEN", "tok123");
        vars
    }

    #[test]
    fn test_simple_substitution() {
        let vars = test_vars();
        let result = render("{{ .ProjectName }}-{{ .Version }}", &vars).unwrap();
        assert_eq!(result, "cfgd-1.2.3");
    }

    #[test]
    fn test_env_access() {
        let vars = test_vars();
        let result = render("{{ .Env.GITHUB_TOKEN }}", &vars).unwrap();
        assert_eq!(result, "tok123");
    }

    #[test]
    fn test_no_spaces() {
        let vars = test_vars();
        let result = render("{{.ProjectName}}-{{.Version}}", &vars).unwrap();
        assert_eq!(result, "cfgd-1.2.3");
    }

    #[test]
    fn test_missing_var() {
        let vars = test_vars();
        let result = render("{{ .Missing }}", &vars);
        assert!(result.is_err());
    }

    #[test]
    fn test_archive_name_template() {
        let vars = test_vars();
        let result = render("{{ .ProjectName }}-{{ .Version }}-{{ .Os }}-{{ .Arch }}", &vars).unwrap();
        assert_eq!(result, "cfgd-1.2.3-linux-amd64");
    }

    #[test]
    fn test_literal_text_preserved() {
        let vars = test_vars();
        let result = render("prefix-{{ .Tag }}-suffix.tar.gz", &vars).unwrap();
        assert_eq!(result, "prefix-v1.2.3-suffix.tar.gz");
    }
}
