// Template rendering powered by Tera.
// Supports both Go-style `{{ .Field }}` and Tera-style `{{ Field }}`.
// Go-style templates are preprocessed (leading dots stripped) before Tera renders them.
// Tera gives us: if/else/endif, for loops, pipes (| lower, | upper, | replace),
// | default, | trim, | title, and many more built-in filters.

use anyhow::{Context as _, Result};
use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;
use tera::Value;

/// Regex to find Go-style dot-prefixed references inside `{{ }}` blocks.
/// Matches `{{ .Field }}`, `{{.Field}}`, `{{ .Env.VAR }}`, and also expressions
/// like `{{ .Field | filter }}`. We only strip the dot from the variable name.
// SAFETY: This is a compile-time regex literal; it is known to be valid.
static GO_DOT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{\{(\s*)\.(\w+)").unwrap());

/// Base Tera instance with custom filters pre-registered.
/// Cloned per render() call (cheap — no templates to clone).
static BASE_TERA: LazyLock<tera::Tera> = LazyLock::new(|| {
    let mut tera = tera::Tera::default();

    // GoReleaser-compat aliases
    tera.register_filter("tolower", |value: &Value, _: &HashMap<String, Value>| {
        let s = tera::try_get_value!("tolower", "value", String, value);
        Ok(Value::String(s.to_lowercase()))
    });
    tera.register_filter("toupper", |value: &Value, _: &HashMap<String, Value>| {
        let s = tera::try_get_value!("toupper", "value", String, value);
        Ok(Value::String(s.to_uppercase()))
    });

    // trimprefix(prefix="...") — strip prefix from a string
    tera.register_filter(
        "trimprefix",
        |value: &Value, args: &HashMap<String, Value>| {
            let s = tera::try_get_value!("trimprefix", "value", String, value);
            let prefix = args
                .get("prefix")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("trimprefix requires a `prefix` argument"))?;
            let result = s.strip_prefix(prefix).unwrap_or(&s);
            Ok(Value::String(result.to_string()))
        },
    );

    // trimsuffix(suffix="...") — strip suffix from a string
    tera.register_filter(
        "trimsuffix",
        |value: &Value, args: &HashMap<String, Value>| {
            let s = tera::try_get_value!("trimsuffix", "value", String, value);
            let suffix = args
                .get("suffix")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("trimsuffix requires a `suffix` argument"))?;
            let result = s.strip_suffix(suffix).unwrap_or(&s);
            Ok(Value::String(result.to_string()))
        },
    );

    // envOrDefault(name="VAR", default="fallback") — return env var value or default
    tera.register_function(
        "envOrDefault",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let name = args
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("envOrDefault requires `name` argument"))?;
            let default = args.get("default").and_then(|v| v.as_str()).unwrap_or("");
            let value = std::env::var(name).unwrap_or_else(|_| default.to_string());
            Ok(Value::String(value))
        },
    );

    // isEnvSet(name="VAR") — return true if env var is set and non-empty
    tera.register_function(
        "isEnvSet",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let name = args
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tera::Error::msg("isEnvSet requires `name` argument"))?;
            let is_set = std::env::var(name).map(|v| !v.is_empty()).unwrap_or(false);
            Ok(Value::Bool(is_set))
        },
    );

    tera
});

#[derive(Clone)]
pub struct TemplateVars {
    vars: HashMap<String, String>,
    env: HashMap<String, String>,
}

impl TemplateVars {
    pub fn new() -> Self {
        Self {
            vars: HashMap::new(),
            env: HashMap::new(),
        }
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

    /// Return all template variables (excluding env).
    pub fn all(&self) -> &HashMap<String, String> {
        &self.vars
    }

    /// Return all environment variables.
    pub fn all_env(&self) -> &HashMap<String, String> {
        &self.env
    }
}

impl Default for TemplateVars {
    fn default() -> Self {
        Self::new()
    }
}

/// Preprocess a template: convert Go-style `{{ .Field }}` to Tera-style `{{ Field }}`.
/// Handles both `{{ .Field }}` and `{{.Field}}` (no spaces).
/// Also handles chained access like `{{ .Env.VAR }}` → `{{ Env.VAR }}`.
fn preprocess(template: &str) -> String {
    // Replace `{{<optional whitespace>.<word>` with `{{<optional whitespace><word>`
    // This strips the leading dot while preserving whitespace and the rest of the expression.
    GO_DOT_RE.replace_all(template, "{{${1}${2}").to_string()
}

/// Build a `tera::Context` from `TemplateVars`.
/// - Regular vars are inserted at the top level: `ProjectName`, `Version`, etc.
/// - Env vars are nested under an `Env` key as a HashMap, so `{{ Env.GITHUB_TOKEN }}` works.
/// - String values of `"true"` / `"false"` are inserted as bools so `{% if Var %}` works.
fn build_tera_context(vars: &TemplateVars) -> tera::Context {
    let mut ctx = tera::Context::new();
    for (k, v) in &vars.vars {
        match v.as_str() {
            "true" => ctx.insert(k.as_str(), &true),
            "false" => ctx.insert(k.as_str(), &false),
            _ => ctx.insert(k.as_str(), v),
        }
    }
    ctx.insert("Env", &vars.env);
    ctx
}

/// Render a template string with the given variables.
///
/// Supports both Go-style (`{{ .Field }}`) and native Tera-style (`{{ Field }}`).
/// Go-style references are preprocessed into Tera-style before rendering.
///
/// Because this uses Tera under the hood, all Tera features are available:
/// conditionals (`{% if %}` / `{% else %}` / `{% endif %}`), loops (`{% for %}`),
/// filters (`| lower`, `| upper`, `| default`, `| trim`, `| title`, `| replace`, etc.).
///
/// Custom GoReleaser-compat filters are registered:
/// - `tolower` / `toupper` — aliases for Tera's built-in `lower` / `upper`
/// - `trimprefix(prefix="v")` — strip a prefix from a string
/// - `trimsuffix(suffix=".exe")` — strip a suffix from a string
pub fn render(template: &str, vars: &TemplateVars) -> Result<String> {
    let preprocessed = preprocess(template);
    let ctx = build_tera_context(vars);

    // Clone the base instance (cheap — filters carry over, no templates to clone)
    let mut tera = BASE_TERA.clone();

    tera.add_raw_template("__inline__", &preprocessed)
        .with_context(|| format!("failed to parse template: {}", template))?;

    tera.render("__inline__", &ctx)
        .with_context(|| format!("failed to render template: {}", template))
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
        let result = render(
            "{{ .ProjectName }}-{{ .Version }}-{{ .Os }}-{{ .Arch }}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "cfgd-1.2.3-linux-amd64");
    }

    #[test]
    fn test_literal_text_preserved() {
        let vars = test_vars();
        let result = render("prefix-{{ .Tag }}-suffix.tar.gz", &vars).unwrap();
        assert_eq!(result, "prefix-v1.2.3-suffix.tar.gz");
    }

    // Tera-style tests (no leading dot)

    #[test]
    fn test_tera_simple_substitution() {
        let vars = test_vars();
        let result = render("{{ ProjectName }}-{{ Version }}", &vars).unwrap();
        assert_eq!(result, "cfgd-1.2.3");
    }

    #[test]
    fn test_tera_env_access() {
        let vars = test_vars();
        let result = render("{{ Env.GITHUB_TOKEN }}", &vars).unwrap();
        assert_eq!(result, "tok123");
    }

    #[test]
    fn test_tera_archive_name() {
        let vars = test_vars();
        let result = render("{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}", &vars).unwrap();
        assert_eq!(result, "cfgd-1.2.3-linux-amd64");
    }

    #[test]
    fn test_tera_missing_var() {
        let vars = test_vars();
        let result = render("{{ Missing }}", &vars);
        assert!(result.is_err());
    }

    // --- Task 1B: custom filters and extended template tests ---

    #[test]
    fn test_conditional_true() {
        let mut vars = test_vars();
        vars.set("IsSnapshot", "true");
        let result = render("{% if IsSnapshot %}SNAP{% endif %}", &vars).unwrap();
        assert_eq!(result, "SNAP");
    }

    #[test]
    fn test_conditional_false_else() {
        let mut vars = test_vars();
        vars.set("IsSnapshot", "false");
        let result = render("{% if IsSnapshot %}SNAP{% else %}RELEASE{% endif %}", &vars).unwrap();
        assert_eq!(result, "RELEASE");
    }

    #[test]
    fn test_pipe_lower() {
        let vars = test_vars();
        let result = render("{{ Version | lower }}", &vars).unwrap();
        assert_eq!(result, "1.2.3");
    }

    #[test]
    fn test_pipe_upper() {
        let vars = test_vars();
        let result = render("{{ ProjectName | upper }}", &vars).unwrap();
        assert_eq!(result, "CFGD");
    }

    #[test]
    fn test_tolower_alias() {
        let vars = test_vars();
        let result = render("{{ ProjectName | tolower }}", &vars).unwrap();
        assert_eq!(result, "cfgd");
    }

    #[test]
    fn test_toupper_alias() {
        let vars = test_vars();
        let result = render("{{ ProjectName | toupper }}", &vars).unwrap();
        assert_eq!(result, "CFGD");
    }

    #[test]
    fn test_trimprefix() {
        let vars = test_vars();
        let result = render("{{ Tag | trimprefix(prefix=\"v\") }}", &vars).unwrap();
        assert_eq!(result, "1.2.3");
    }

    #[test]
    fn test_trimprefix_no_match() {
        let vars = test_vars();
        let result = render("{{ Tag | trimprefix(prefix=\"x\") }}", &vars).unwrap();
        assert_eq!(result, "v1.2.3");
    }

    #[test]
    fn test_trimsuffix() {
        let vars = test_vars();
        let result = render("{{ ProjectName | trimsuffix(suffix=\"gd\") }}", &vars).unwrap();
        assert_eq!(result, "cf");
    }

    #[test]
    fn test_trimsuffix_no_match() {
        let vars = test_vars();
        let result = render("{{ ProjectName | trimsuffix(suffix=\"xyz\") }}", &vars).unwrap();
        assert_eq!(result, "cfgd");
    }

    #[test]
    fn test_default_value_for_undefined() {
        let vars = test_vars();
        let result = render("{{ Undefined | default(value=\"fallback\") }}", &vars).unwrap();
        assert_eq!(result, "fallback");
    }

    #[test]
    fn test_bad_syntax_error() {
        let vars = test_vars();
        let result = render("{{ unclosed", &vars);
        assert!(result.is_err());
    }

    #[test]
    fn test_nested_env_conditional() {
        let vars = test_vars();
        let result = render("{% if Env.GITHUB_TOKEN %}has token{% endif %}", &vars).unwrap();
        assert_eq!(result, "has token");
    }

    #[test]
    fn test_trimprefix_missing_arg_error() {
        let vars = test_vars();
        let result = render("{{ Tag | trimprefix }}", &vars);
        assert!(result.is_err());
    }

    #[test]
    fn test_trimsuffix_missing_arg_error() {
        let vars = test_vars();
        let result = render("{{ Tag | trimsuffix }}", &vars);
        assert!(result.is_err());
    }

    #[test]
    fn test_filter_chaining() {
        let vars = test_vars();
        let result = render("{{ Tag | trimprefix(prefix=\"v\") | upper }}", &vars).unwrap();
        assert_eq!(result, "1.2.3");
    }

    // ---- Error path tests (Task 3B) ----

    #[test]
    fn test_unknown_filter_error() {
        let vars = test_vars();
        let result = render("{{ ProjectName | nonexistent_filter }}", &vars);
        assert!(result.is_err(), "unknown filter should produce an error");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("nonexistent_filter"),
            "error should mention the unknown filter name, got: {err}"
        );
    }

    #[test]
    fn test_unclosed_block_tag_error() {
        let vars = test_vars();
        let result = render("{% if ProjectName %} hello", &vars);
        assert!(result.is_err(), "unclosed if block should produce an error");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("template") || err.contains("if"),
            "error should reference the template or block tag, got: {err}"
        );
    }

    #[test]
    fn test_trailing_pipe_with_no_filter_name_error() {
        let vars = test_vars();
        // A trailing pipe with no filter name is a distinct syntax error from
        // just an unclosed tag (which test_bad_syntax_error already covers).
        let result = render("{{ ProjectName | }}", &vars);
        assert!(
            result.is_err(),
            "trailing pipe with no filter name should produce an error"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("parse") || err.contains("unexpected") || err.contains("template"),
            "error should mention a parsing issue, got: {err}"
        );
    }

    #[test]
    fn test_nested_missing_var_in_expression_error() {
        let vars = test_vars();
        // Using an undefined variable in an expression (not just a conditional
        // truthiness check) should error when the template tries to render it.
        let result = render("{{ Undefined ~ ' suffix' }}", &vars);
        assert!(
            result.is_err(),
            "undefined variable in an expression should produce an error"
        );
    }

    #[test]
    fn test_invalid_filter_argument_type_error() {
        let vars = test_vars();
        // trimprefix expects prefix=<string>, but we pass an unquoted value
        // that Tera will interpret differently
        let result = render("{{ Tag | trimprefix(prefix=123) }}", &vars);
        assert!(
            result.is_err(),
            "invalid filter argument type should produce an error"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("trimprefix") || err.contains("prefix") || err.contains("argument"),
            "error should mention the filter or argument, got: {err}"
        );
    }

    #[test]
    fn test_error_message_includes_original_template() {
        let vars = test_vars();
        let template = "{{ .Nonexistent }}";
        let result = render(template, &vars);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        // Our render() adds context with the original template
        assert!(
            err.contains("Nonexistent") || err.contains(template),
            "error should reference the template or variable name, got: {err}"
        );
    }

    #[test]
    fn test_mismatched_endfor_with_if_error() {
        let vars = test_vars();
        let result = render("{% if ProjectName %}hello{% endfor %}", &vars);
        assert!(
            result.is_err(),
            "mismatched block tags should produce an error"
        );
    }

    // ---- Error path tests (Task 4D) ----

    #[test]
    fn test_undefined_variable_error_mentions_variable() {
        let vars = test_vars();
        let result = render("{{ UndefinedFoo }}", &vars);
        assert!(
            result.is_err(),
            "undefined variable should produce an error"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("UndefinedFoo") || err.contains("template"),
            "error should mention the undefined variable name, got: {err}"
        );
    }

    #[test]
    fn test_unclosed_brace_syntax_error() {
        let vars = test_vars();
        let result = render("{{ ProjectName", &vars);
        assert!(result.is_err(), "unclosed brace should produce an error");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("parse") || err.contains("template") || err.contains("ProjectName"),
            "error should indicate a parse failure, got: {err}"
        );
    }

    #[test]
    fn test_unclosed_tag_block_error() {
        let vars = test_vars();
        let result = render("{% for x in items %} content", &vars);
        assert!(
            result.is_err(),
            "unclosed for block should produce an error"
        );
    }

    #[test]
    fn test_invalid_filter_name_error_mentions_filter() {
        let vars = test_vars();
        let result = render("{{ ProjectName | bogus_filter_name }}", &vars);
        assert!(result.is_err(), "invalid filter should produce an error");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("bogus_filter_name"),
            "error should mention the invalid filter name, got: {err}"
        );
    }

    #[test]
    fn test_deeply_nested_undefined_variable_error() {
        let vars = test_vars();
        let result = render("{{ Env.NONEXISTENT_VAR_12345 }}", &vars);
        // Env is defined but NONEXISTENT_VAR_12345 is not a key in it.
        // Tera treats this as an undefined variable and returns an error.
        assert!(
            result.is_err(),
            "accessing a missing key in a map should produce an error"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("NONEXISTENT_VAR_12345") || err.contains("Env"),
            "error should reference the undefined variable, got: {err}"
        );
    }

    #[test]
    fn test_go_style_syntax_error_reports_original_template() {
        let vars = test_vars();
        let template = "{{ .Missing | bad_filter }}";
        let result = render(template, &vars);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        // The error context added by render() should include the original template
        assert!(
            err.contains("bad_filter") || err.contains(template),
            "error should reference the original template or filter, got: {err}"
        );
    }

    #[test]
    fn test_empty_template_renders_empty() {
        let vars = test_vars();
        let result = render("", &vars);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "");
    }

    #[test]
    fn test_multiple_errors_in_template() {
        let vars = test_vars();
        // This template has both an undefined variable and a syntax issue
        let result = render("{% if %}", &vars);
        assert!(
            result.is_err(),
            "empty if condition should produce an error"
        );
    }

    // ---- envOrDefault and isEnvSet function tests ----

    #[test]
    fn test_env_or_default_returns_env_value_when_set() {
        let vars = test_vars();
        // SAFETY: Test-only; no other threads read this env var.
        unsafe { std::env::set_var("ANODIZE_TEST_ENV_OR_DEFAULT", "from-env") };
        let result = render(
            "{{ envOrDefault(name=\"ANODIZE_TEST_ENV_OR_DEFAULT\", default=\"fallback\") }}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "from-env");
        unsafe { std::env::remove_var("ANODIZE_TEST_ENV_OR_DEFAULT") };
    }

    #[test]
    fn test_env_or_default_returns_default_when_unset() {
        let vars = test_vars();
        // SAFETY: Test-only; no other threads read this env var.
        unsafe { std::env::remove_var("ANODIZE_TEST_UNSET_VAR_XYZ") };
        let result = render(
            "{{ envOrDefault(name=\"ANODIZE_TEST_UNSET_VAR_XYZ\", default=\"fallback\") }}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "fallback");
    }

    #[test]
    fn test_env_or_default_returns_empty_when_no_default() {
        let vars = test_vars();
        // SAFETY: Test-only; no other threads read this env var.
        unsafe { std::env::remove_var("ANODIZE_TEST_UNSET_VAR_XYZ2") };
        let result = render(
            "{{ envOrDefault(name=\"ANODIZE_TEST_UNSET_VAR_XYZ2\") }}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn test_env_or_default_missing_name_error() {
        let vars = test_vars();
        let result = render("{{ envOrDefault(default=\"x\") }}", &vars);
        assert!(result.is_err(), "envOrDefault without name should error");
    }

    #[test]
    fn test_is_env_set_true_when_set() {
        let vars = test_vars();
        // SAFETY: Test-only; no other threads read this env var.
        unsafe { std::env::set_var("ANODIZE_TEST_IS_SET", "yes") };
        let result = render(
            "{% if isEnvSet(name=\"ANODIZE_TEST_IS_SET\") %}SET{% else %}UNSET{% endif %}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "SET");
        unsafe { std::env::remove_var("ANODIZE_TEST_IS_SET") };
    }

    #[test]
    fn test_is_env_set_false_when_unset() {
        let vars = test_vars();
        // SAFETY: Test-only; no other threads read this env var.
        unsafe { std::env::remove_var("ANODIZE_TEST_NOT_SET_XYZ") };
        let result = render(
            "{% if isEnvSet(name=\"ANODIZE_TEST_NOT_SET_XYZ\") %}SET{% else %}UNSET{% endif %}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "UNSET");
    }

    #[test]
    fn test_is_env_set_false_when_empty() {
        let vars = test_vars();
        // SAFETY: Test-only; no other threads read this env var.
        unsafe { std::env::set_var("ANODIZE_TEST_EMPTY_VAR", "") };
        let result = render(
            "{% if isEnvSet(name=\"ANODIZE_TEST_EMPTY_VAR\") %}SET{% else %}UNSET{% endif %}",
            &vars,
        )
        .unwrap();
        assert_eq!(result, "UNSET");
        unsafe { std::env::remove_var("ANODIZE_TEST_EMPTY_VAR") };
    }

    #[test]
    fn test_is_env_set_missing_name_error() {
        let vars = test_vars();
        let result = render("{{ isEnvSet() }}", &vars);
        assert!(result.is_err(), "isEnvSet without name should error");
    }
}
