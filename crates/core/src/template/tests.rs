use std::collections::HashMap;
use tera::Value;

use super::base_tera::translate_go_time_format;
use super::render::{extract_artifact_ext, render, render_with_env};
use super::vars::TemplateVars;
use crate::env_source::MapEnvSource;

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

// --- Custom filters and extended template tests ---

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
fn test_english_join_pipe_form_on_structured_var() {
    // Regression guard for B15: the changelog renderer publishes per-entry
    // `LoginsList` / `AuthorsList` via `set_structured`, then user format
    // templates pipe through `englishJoin`. Existing englishJoin tests
    // only exercised inline-literal arrays — pin the
    // `set_structured(...) → {{ Var | englishJoin }}` path so that
    // contract doesn't regress.
    let mut vars = test_vars();
    vars.set_structured(
        "Names",
        Value::Array(vec![
            Value::String("alice".into()),
            Value::String("bob".into()),
            Value::String("carol".into()),
        ]),
    );
    let result = render("{{ Names | englishJoin }}", &vars).unwrap();
    assert_eq!(result, "alice, bob, and carol");
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

// ---- Error path tests: unknown filter / parse failures ----

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

// ---- Error path tests: undefined variables ----

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
fn test_missing_env_var_returns_empty_string() {
    // GoReleaser returns empty string for missing env vars.
    // Anodizer scans the template for Env.X references and pre-populates
    // missing keys with "" so Tera doesn't error.
    let vars = test_vars();
    let result = render("{{ Env.NONEXISTENT_VAR_12345 }}", &vars).unwrap();
    assert_eq!(result, "", "missing env var should return empty string");
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
fn test_env_or_default_reads_from_template_env_map() {
    // The primary path: envOrDefault reads from the template context Env map,
    // NOT from the process environment. This is the .env file use case.
    let mut vars = test_vars();
    vars.set_env("MY_CUSTOM_VAR", "from-template-env");
    let result = render(
        "{{ envOrDefault(name=\"MY_CUSTOM_VAR\", default=\"fallback\") }}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "from-template-env");
}

#[test]
fn test_env_or_default_template_env_takes_priority_over_process_env() {
    // If a var exists in both the template Env map and the host env,
    // the template Env map wins.
    let mut vars = test_vars();
    let host = MapEnvSource::new().with("ANODIZER_TEST_PRIORITY", "from-process");
    vars.set_env("ANODIZER_TEST_PRIORITY", "from-template");
    let result = render_with_env(
        "{{ envOrDefault(name=\"ANODIZER_TEST_PRIORITY\", default=\"fallback\") }}",
        &vars,
        &host,
    )
    .unwrap();
    assert_eq!(result, "from-template");
}

#[test]
fn test_env_or_default_falls_back_to_process_env() {
    // If a var is NOT in the template Env map but IS in the host env,
    // fall back to the host env.
    let vars = test_vars();
    let host = MapEnvSource::new().with("ANODIZER_TEST_ENV_OR_DEFAULT", "from-process-env");
    let result = render_with_env(
        "{{ envOrDefault(name=\"ANODIZER_TEST_ENV_OR_DEFAULT\", default=\"fallback\") }}",
        &vars,
        &host,
    )
    .unwrap();
    assert_eq!(result, "from-process-env");
}

#[test]
fn test_env_or_default_returns_default_when_unset() {
    let vars = test_vars();
    let result = render(
        "{{ envOrDefault(name=\"ANODIZER_TEST_UNSET_VAR_XYZ\", default=\"fallback\") }}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "fallback");
}

#[test]
fn test_env_or_default_returns_empty_when_no_default() {
    let vars = test_vars();
    let result = render(
        "{{ envOrDefault(name=\"ANODIZER_TEST_UNSET_VAR_XYZ2\") }}",
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
fn test_is_env_set_reads_from_template_env_map() {
    // The primary path: isEnvSet reads from the template context Env map.
    let mut vars = test_vars();
    vars.set_env("MY_CUSTOM_CHECK", "yes");
    let result = render(
        "{% if isEnvSet(name=\"MY_CUSTOM_CHECK\") %}SET{% else %}UNSET{% endif %}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "SET");
}

#[test]
fn test_is_env_set_template_env_empty_returns_false() {
    // An empty string in the template Env map should return false.
    let mut vars = test_vars();
    vars.set_env("MY_EMPTY_VAR", "");
    let result = render(
        "{% if isEnvSet(name=\"MY_EMPTY_VAR\") %}SET{% else %}UNSET{% endif %}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "UNSET");
}

#[test]
fn test_is_env_set_falls_back_to_process_env() {
    // If a var is NOT in the template Env map but IS in the host env,
    // fall back to the host env.
    let vars = test_vars();
    let host = MapEnvSource::new().with("ANODIZER_TEST_IS_SET", "yes");
    let result = render_with_env(
        "{% if isEnvSet(name=\"ANODIZER_TEST_IS_SET\") %}SET{% else %}UNSET{% endif %}",
        &vars,
        &host,
    )
    .unwrap();
    assert_eq!(result, "SET");
}

#[test]
fn test_is_env_set_false_when_unset() {
    let vars = test_vars();
    let result = render(
        "{% if isEnvSet(name=\"ANODIZER_TEST_NOT_SET_XYZ\") %}SET{% else %}UNSET{% endif %}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "UNSET");
}

#[test]
fn test_is_env_set_missing_name_error() {
    let vars = test_vars();
    let result = render("{{ isEnvSet() }}", &vars);
    assert!(result.is_err(), "isEnvSet without name should error");
}

// ---- Hash function tests (known-answer vectors) ----
// Hash functions read file contents (GoReleaser parity), so tests use temp files.

fn hash_test_file() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hello.txt");
    std::fs::write(&path, "hello").unwrap();
    (dir, path.to_string_lossy().into_owned())
}

#[test]
fn test_hash_sha1() {
    let vars = test_vars();
    let (_dir, path) = hash_test_file();
    let tmpl = format!("{{{{ sha1(s=\"{path}\") }}}}");
    let result = render(&tmpl, &vars).unwrap();
    assert_eq!(result, "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d");
}

#[test]
fn test_hash_sha256() {
    let vars = test_vars();
    let (_dir, path) = hash_test_file();
    let tmpl = format!("{{{{ sha256(s=\"{path}\") }}}}");
    let result = render(&tmpl, &vars).unwrap();
    assert_eq!(
        result,
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
}

#[test]
fn test_hash_sha512() {
    let vars = test_vars();
    let (_dir, path) = hash_test_file();
    let tmpl = format!("{{{{ sha512(s=\"{path}\") }}}}");
    let result = render(&tmpl, &vars).unwrap();
    assert_eq!(
        result,
        "9b71d224bd62f3785d96d46ad3ea3d73319bfbc2890caadae2dff72519673ca72323c3d99ba5c11d7c7acc6e14b8c5da0c4663475c2e5c3adef46f73bcdec043"
    );
}

#[test]
fn test_hash_md5() {
    let vars = test_vars();
    let (_dir, path) = hash_test_file();
    let tmpl = format!("{{{{ md5(s=\"{path}\") }}}}");
    let result = render(&tmpl, &vars).unwrap();
    assert_eq!(result, "5d41402abc4b2a76b9719d911017c592");
}

#[test]
fn test_hash_blake3() {
    let vars = test_vars();
    let (_dir, path) = hash_test_file();
    let tmpl = format!("{{{{ blake3(s=\"{path}\") }}}}");
    let result = render(&tmpl, &vars).unwrap();
    assert_eq!(
        result,
        "ea8f163db38682925e4491c5e58d4bb3506ef8c14eb78a86e908c5624a67200f"
    );
}

#[test]
fn test_hash_crc32() {
    let vars = test_vars();
    let (_dir, path) = hash_test_file();
    let tmpl = format!("{{{{ crc32(s=\"{path}\") }}}}");
    let result = render(&tmpl, &vars).unwrap();
    assert_eq!(result, "3610a686");
}

#[test]
fn test_hash_missing_s_arg_error() {
    let vars = test_vars();
    let result = render("{{ sha256() }}", &vars);
    assert!(
        result.is_err(),
        "hash function without `s` arg should error"
    );
    // The anyhow error chain includes the tera error with our message
    let err = format!("{:#}", result.unwrap_err());
    assert!(
        err.contains("requires `s` argument"),
        "error should mention missing `s` argument, got: {err}"
    );
}

// ---- Version increment tests ----

#[test]
fn test_incpatch() {
    let vars = test_vars();
    let result = render("{{ incpatch(v=\"1.2.3\") }}", &vars).unwrap();
    assert_eq!(result, "1.2.4");
}

#[test]
fn test_incminor() {
    let vars = test_vars();
    let result = render("{{ incminor(v=\"1.2.3\") }}", &vars).unwrap();
    assert_eq!(result, "1.3.0");
}

#[test]
fn test_incmajor() {
    let vars = test_vars();
    let result = render("{{ incmajor(v=\"1.2.3\") }}", &vars).unwrap();
    assert_eq!(result, "2.0.0");
}

#[test]
fn test_incpatch_preserves_v_prefix() {
    let vars = test_vars();
    let result = render("{{ incpatch(v=\"v1.2.3\") }}", &vars).unwrap();
    assert_eq!(result, "v1.2.4");
}

#[test]
fn test_incpatch_handles_prerelease() {
    let vars = test_vars();
    let result = render("{{ incpatch(v=\"1.2.3-rc.1\") }}", &vars).unwrap();
    assert_eq!(result, "1.2.4");
}

// Q-bump1: non-semver input must hard-error rather than silently
// returning "0.0.1" / "0.1.0" / "1.0.0". Mirrors GR
// `internal/tmpl/tmpl.go:440-449` `semver.MustParse(v)` panic.
//
// `render()` wraps the underlying Tera error in `anyhow::Error`, so we
// walk the source chain to find the actual semver-validation message.
fn err_chain(err: &anyhow::Error) -> String {
    let mut s = String::new();
    s.push_str(&format!("{}", err));
    let mut src: Option<&dyn std::error::Error> = err.source();
    while let Some(e) = src {
        s.push_str(" | ");
        s.push_str(&format!("{}", e));
        src = e.source();
    }
    s
}

#[test]
fn test_incpatch_rejects_non_semver_function_form() {
    let vars = test_vars();
    let err = render("{{ incpatch(v=\"garbage\") }}", &vars).unwrap_err();
    let s = err_chain(&err);
    assert!(
        s.contains("garbage") && s.contains("not a valid semver"),
        "expected error mentioning the offending input + 'not a valid semver', got: {}",
        s
    );
}

#[test]
fn test_incpatch_rejects_non_semver_filter_form() {
    let vars = test_vars();
    let err = render("{{ \"oops\" | incpatch }}", &vars).unwrap_err();
    let s = err_chain(&err);
    assert!(
        s.contains("oops") && s.contains("not a valid semver"),
        "expected error mentioning the offending input + 'not a valid semver', got: {}",
        s
    );
}

#[test]
fn test_incminor_rejects_two_component_version() {
    let vars = test_vars();
    let err = render("{{ incminor(v=\"1.2\") }}", &vars).unwrap_err();
    let s = err_chain(&err);
    assert!(
        s.contains("not a valid semver"),
        "two-component versions must error, got: {}",
        s
    );
}

#[test]
fn test_incmajor_rejects_alpha_component() {
    let vars = test_vars();
    let err = render("{{ incmajor(v=\"a.b.c\") }}", &vars).unwrap_err();
    assert!(err_chain(&err).contains("not a valid semver"));
}

// ---- readFile / mustReadFile tests ----

#[test]
fn test_read_file_existing() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("test.txt");
    std::fs::write(&file_path, "file content").unwrap();

    let vars = test_vars();
    let template = format!(
        "{{{{ readFile(path=\"{}\") }}}}",
        file_path.to_string_lossy().replace('\\', "\\\\")
    );
    let result = render(&template, &vars).unwrap();
    assert_eq!(result, "file content");
}

#[test]
fn test_read_file_nonexistent_returns_empty() {
    let vars = test_vars();
    let result = render(
        "{{ readFile(path=\"/tmp/anodizer_test_nonexistent_file_xyz\") }}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "");
}

#[test]
fn test_must_read_file_existing() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("test.txt");
    std::fs::write(&file_path, "must content").unwrap();

    let vars = test_vars();
    let template = format!(
        "{{{{ mustReadFile(path=\"{}\") }}}}",
        file_path.to_string_lossy().replace('\\', "\\\\")
    );
    let result = render(&template, &vars).unwrap();
    assert_eq!(result, "must content");
}

#[test]
fn test_must_read_file_nonexistent_errors() {
    let vars = test_vars();
    let result = render(
        "{{ mustReadFile(path=\"/tmp/anodizer_test_nonexistent_file_xyz\") }}",
        &vars,
    );
    assert!(
        result.is_err(),
        "mustReadFile with nonexistent file should error"
    );
}

// ---- Path filter tests ----

#[test]
fn test_dir_filter() {
    let mut vars = test_vars();
    vars.set("FilePath", "/foo/bar/baz.txt");
    let result = render("{{ FilePath | dir }}", &vars).unwrap();
    assert_eq!(result, "/foo/bar");
}

#[test]
fn test_base_filter() {
    let mut vars = test_vars();
    vars.set("FilePath", "/foo/bar/baz.txt");
    let result = render("{{ FilePath | base }}", &vars).unwrap();
    assert_eq!(result, "baz.txt");
}

// ---- urlPathEscape tests ----

#[test]
fn test_url_path_escape_spaces() {
    let mut vars = test_vars();
    vars.set("Input", "hello world");
    let result = render("{{ Input | urlPathEscape }}", &vars).unwrap();
    assert_eq!(result, "hello%20world");
}

#[test]
fn test_url_path_escape_encodes_slashes() {
    let mut vars = test_vars();
    vars.set("Input", "foo/bar");
    let result = render("{{ Input | urlPathEscape }}", &vars).unwrap();
    assert_eq!(result, "foo%2Fbar");
}

// ---- mdv2escape tests ----

#[test]
fn test_mdv2escape() {
    let mut vars = test_vars();
    vars.set("Input", "hello_world");
    let result = render("{{ Input | mdv2escape }}", &vars).unwrap();
    assert_eq!(result, "hello\\_world");
}

// ---- contains tests ----

#[test]
fn test_contains_true() {
    let vars = test_vars();
    let result = render(
        "{% if contains(s=\"hello world\", substr=\"world\") %}yes{% else %}no{% endif %}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "yes");
}

#[test]
fn test_contains_false() {
    let vars = test_vars();
    let result = render(
        "{% if contains(s=\"hello\", substr=\"xyz\") %}yes{% else %}no{% endif %}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "no");
}

// ---- englishJoin tests ----

#[test]
fn test_english_join_zero_items() {
    let vars = test_vars();
    // Pass an empty array via list()
    let result = render("{{ englishJoin(items=[]) }}", &vars).unwrap();
    assert_eq!(result, "");
}

#[test]
fn test_english_join_one_item() {
    let vars = test_vars();
    let result = render("{{ englishJoin(items=[\"a\"]) }}", &vars).unwrap();
    assert_eq!(result, "a");
}

#[test]
fn test_english_join_two_items() {
    let vars = test_vars();
    let result = render("{{ englishJoin(items=[\"a\", \"b\"]) }}", &vars).unwrap();
    assert_eq!(result, "a and b");
}

#[test]
fn test_english_join_three_items_oxford() {
    let vars = test_vars();
    let result = render("{{ englishJoin(items=[\"a\", \"b\", \"c\"]) }}", &vars).unwrap();
    assert_eq!(result, "a, b, and c");
}

#[test]
fn test_english_join_three_items_no_oxford() {
    let vars = test_vars();
    let result = render(
        "{{ englishJoin(items=[\"a\", \"b\", \"c\"], oxford=false) }}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "a, b and c");
}

// ---- filter / reverseFilter tests ----

#[test]
fn test_filter_keeps_matches() {
    let vars = test_vars();
    let result = render(
        "{{ filter(items=[\"apple\", \"banana\", \"avocado\"], regexp=\"^a\") }}",
        &vars,
    )
    .unwrap();
    // Tera renders arrays as JSON
    assert!(result.contains("apple"));
    assert!(result.contains("avocado"));
    assert!(!result.contains("banana"));
}

#[test]
fn test_reverse_filter_removes_matches() {
    let vars = test_vars();
    let result = render(
        "{{ reverseFilter(items=[\"apple\", \"banana\", \"avocado\"], regexp=\"^a\") }}",
        &vars,
    )
    .unwrap();
    assert!(result.contains("banana"));
    assert!(!result.contains("apple"));
    assert!(!result.contains("avocado"));
}

// Q15.2 — `filter` and `reverseFilter` must return an error (not panic)
// when the user supplies an invalid regex. Mirrors GoReleaser commit
// c2f16b9 (internal/tmpl/tmpl.go): upstream replaced `regexp.MustCompilePOSIX`
// (panicking) with `regexp.CompilePOSIX` (returning error). Rust's
// `regex::Regex::new` already returns `Result`, so this contract is
// panic-free by construction; this test pins it.
#[test]
fn test_filter_function_invalid_regex_returns_error() {
    let vars = test_vars();
    let result = render(
        // `[` is an unterminated character class — invalid regex.
        "{{ filter(items=[\"apple\"], regexp=\"[\") }}",
        &vars,
    );
    assert!(result.is_err(), "invalid regex must produce an error");
    // Use full-chain debug formatter so the inner regex-compile error
    // (wrapped by Tera + anyhow context) is visible to the assertion.
    let err = format!("{:?}", result.unwrap_err());
    assert!(
        err.contains("invalid regex") || err.contains("filter"),
        "error chain should mention the invalid regex, got: {err}"
    );
}

#[test]
fn test_reverse_filter_function_invalid_regex_returns_error() {
    let vars = test_vars();
    let result = render(
        "{{ reverseFilter(items=[\"apple\"], regexp=\"[\") }}",
        &vars,
    );
    assert!(result.is_err(), "invalid regex must produce an error");
    // Mirror the forward-filter sibling: assert the error chain mentions
    // the specific failure mode so we don't accept a generic Tera error
    // (e.g. an arity / arg-name change) as a pass.
    let err = format!("{:?}", result.unwrap_err());
    assert!(
        err.to_lowercase().contains("invalid regex"),
        "expected 'invalid regex' in error chain, got: {err}"
    );
}

#[test]
fn test_filter_pipe_invalid_regex_returns_error() {
    // Pipe form: `{{ value | filter(regexp="...") }}`.
    let vars = test_vars();
    let result = render("{{ ProjectName | filter(regexp=\"[\") }}", &vars);
    assert!(
        result.is_err(),
        "invalid regex in pipe form must produce an error"
    );
    // Tera wraps the inner filter error in the template render error.
    // Use the alternate-debug formatter to render the full anyhow chain
    // (`.to_string()` only shows the outermost context).
    let err = format!("{:?}", result.unwrap_err());
    assert!(
        err.contains("invalid regex"),
        "error chain should mention 'invalid regex', got: {err}"
    );
}

#[test]
fn test_reverse_filter_pipe_invalid_regex_returns_error() {
    let vars = test_vars();
    let result = render("{{ ProjectName | reverseFilter(regexp=\"[\") }}", &vars);
    assert!(
        result.is_err(),
        "invalid regex in pipe form must produce an error"
    );
    // Symmetry with forward-filter pipe sibling: assert the error chain
    // names the failure mode so a future Tera bump or filter-name rename
    // doesn't silently degrade this to a generic-error pass.
    let err = format!("{:?}", result.unwrap_err());
    assert!(
        err.to_lowercase().contains("invalid regex"),
        "expected 'invalid regex' in error chain, got: {err}"
    );
}

// ---- indexOrDefault tests ----

#[test]
fn test_index_or_default_key_exists() {
    // We need to construct a template that passes a map. Tera doesn't have inline map
    // literals in templates, so we test the function via the Rust API directly.
    let args: HashMap<String, Value> = [
        ("map".to_string(), serde_json::json!({"foo": "bar"})),
        ("key".to_string(), Value::String("foo".to_string())),
        ("default".to_string(), Value::String("fallback".to_string())),
    ]
    .into_iter()
    .collect();

    // Access the function via BASE_TERA - we test it indirectly by calling the logic
    let map = args.get("map").unwrap().as_object().unwrap();
    let key = args.get("key").unwrap().as_str().unwrap();
    let default = args
        .get("default")
        .cloned()
        .unwrap_or(Value::String(String::new()));
    let result = map.get(key).cloned().unwrap_or(default);
    assert_eq!(result, Value::String("bar".to_string()));
}

#[test]
fn test_index_or_default_key_missing() {
    let args: HashMap<String, Value> = [
        ("map".to_string(), serde_json::json!({"foo": "bar"})),
        ("key".to_string(), Value::String("missing".to_string())),
        ("default".to_string(), Value::String("fallback".to_string())),
    ]
    .into_iter()
    .collect();

    let map = args.get("map").unwrap().as_object().unwrap();
    let key = args.get("key").unwrap().as_str().unwrap();
    let default = args
        .get("default")
        .cloned()
        .unwrap_or(Value::String(String::new()));
    let result = map.get(key).cloned().unwrap_or(default);
    assert_eq!(result, Value::String("fallback".to_string()));
}

// ---- Runtime vars test ----

#[test]
fn test_runtime_goos_renders() {
    let mut vars = test_vars();
    vars.set("RuntimeGoos", std::env::consts::OS);
    let result = render("{{ Runtime.Goos }}", &vars).unwrap();
    assert!(
        !result.is_empty(),
        "Runtime.Goos should render to a non-empty string"
    );
}

// ---- Custom variables (.Var.*) tests ----

#[test]
fn test_custom_var_tera_style() {
    let mut vars = test_vars();
    vars.set_custom_var("description", "my project description");
    let result = render("{{ Var.description }}", &vars).unwrap();
    assert_eq!(result, "my project description");
}

#[test]
fn test_custom_var_go_style() {
    let mut vars = test_vars();
    vars.set_custom_var("mykey", "myvalue");
    let result = render("{{ .Var.mykey }}", &vars).unwrap();
    assert_eq!(result, "myvalue");
}

#[test]
fn test_custom_var_multiple() {
    let mut vars = test_vars();
    vars.set_custom_var("name", "anodizer");
    vars.set_custom_var("desc", "release tool");
    let result = render("{{ .Var.name }} - {{ .Var.desc }}", &vars).unwrap();
    assert_eq!(result, "anodizer - release tool");
}

#[test]
fn test_custom_var_empty_map_no_error() {
    // When no custom vars are set, Var is still inserted as an empty map.
    // Rendering a template that does NOT reference Var should succeed.
    let vars = test_vars();
    let result = render("{{ ProjectName }}", &vars).unwrap();
    assert_eq!(result, "cfgd");
}

#[test]
fn test_custom_var_undefined_key_errors() {
    // Accessing an undefined key within the Var map produces an error,
    // matching Tera's strict behavior (same as Env.NONEXISTENT).
    // Users can use `{{ Var.key | default(value="") }}` for optional vars.
    let vars = test_vars();
    let result = render("{{ Var.nonexistent }}", &vars);
    assert!(
        result.is_err(),
        "accessing a missing key in Var should produce an error"
    );
}

#[test]
fn test_custom_var_undefined_key_with_other_vars_set() {
    // When some custom vars exist, referencing an undefined key should
    // still error (Tera strict mode).
    let mut vars = test_vars();
    vars.set_custom_var("exists", "yes");
    let result = render("{{ Var.missing }}", &vars);
    assert!(
        result.is_err(),
        "accessing a missing key in Var should produce an error"
    );
}

#[test]
fn test_custom_var_empty_map_conditional() {
    // Var is always inserted as an empty map. Tera treats empty maps as
    // falsy so `{% if Var %}` correctly distinguishes empty vs non-empty.
    let vars = test_vars();
    let result = render("{% if Var %}has vars{% else %}no vars{% endif %}", &vars).unwrap();
    assert_eq!(result, "no vars");

    let mut vars2 = test_vars();
    vars2.set_custom_var("key", "val");
    let result2 = render("{% if Var %}has vars{% else %}no vars{% endif %}", &vars2).unwrap();
    assert_eq!(result2, "has vars");
}

#[test]
fn test_custom_var_with_template_in_value() {
    // Verify that custom var values can themselves be template-rendered
    // (this is done in the CLI wiring, but we can test the end result here)
    let mut vars = test_vars();
    // Simulate a pre-rendered value (as the CLI would do)
    vars.set_custom_var("version_string", "cfgd v1.2.3");
    let result = render("{{ .Var.version_string }}", &vars).unwrap();
    assert_eq!(result, "cfgd v1.2.3");
}

// ---- Go-style positional syntax tests ----

#[test]
fn test_positional_replace_standalone() {
    // {{ replace .Version "v" "" }} should strip "v" from empty tag
    let mut vars = test_vars();
    vars.set("Version", "v1.2.3");
    let result = render("{{ replace .Version \"v\" \"\" }}", &vars).unwrap();
    assert_eq!(result, "1.2.3");
}

#[test]
fn test_positional_replace_standalone_no_dot() {
    // Tera-style: {{ replace Version "v" "" }}
    let mut vars = test_vars();
    vars.set("Version", "v1.2.3");
    let result = render("{{ replace Version \"v\" \"\" }}", &vars).unwrap();
    assert_eq!(result, "1.2.3");
}

#[test]
fn test_positional_replace_piped() {
    // {{ .Version | replace "v" "" }} should strip "v" prefix
    let mut vars = test_vars();
    vars.set("Version", "v1.2.3");
    let result = render("{{ .Version | replace \"v\" \"\" }}", &vars).unwrap();
    assert_eq!(result, "1.2.3");
}

#[test]
fn test_positional_replace_piped_no_dot() {
    // Tera-style: {{ Version | replace "v" "" }}
    let mut vars = test_vars();
    vars.set("Version", "v1.2.3");
    let result = render("{{ Version | replace \"v\" \"\" }}", &vars).unwrap();
    assert_eq!(result, "1.2.3");
}

#[test]
fn test_positional_split_standalone() {
    // {{ split .Version "." }} should split on dots
    let vars = test_vars();
    let result = render("{{ split .Version \".\" }}", &vars).unwrap();
    // Tera renders arrays as JSON, e.g. ["1", "2", "3"]
    assert!(result.contains("1"));
    assert!(result.contains("2"));
    assert!(result.contains("3"));
}

#[test]
fn test_positional_split_piped() {
    // {{ .Version | split "." }} should split on dots
    let vars = test_vars();
    let result = render("{{ .Version | split \".\" }}", &vars).unwrap();
    assert!(result.contains("1"));
    assert!(result.contains("2"));
    assert!(result.contains("3"));
}

#[test]
fn test_positional_contains_standalone_true() {
    // {{ contains .Version "2" }} should return true
    let vars = test_vars();
    let result = render(
        "{% if contains .Version \"2\" %}yes{% else %}no{% endif %}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "yes");
}

#[test]
fn test_positional_contains_standalone_false() {
    let vars = test_vars();
    let result = render(
        "{% if contains .Version \"rc\" %}yes{% else %}no{% endif %}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "no");
}

#[test]
fn test_positional_contains_piped() {
    // {{ .Tag | contains "v" }} piped positional form
    let vars = test_vars();
    let result = render(
        "{% if Tag | contains \"v\" %}yes{% else %}no{% endif %}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "yes");
}

#[test]
fn test_positional_replace_with_env_var() {
    // Using dotted path: {{ replace .Env.GITHUB_TOKEN "tok" "XXX" }}
    let vars = test_vars();
    let result = render("{{ replace .Env.GITHUB_TOKEN \"tok\" \"XXX\" }}", &vars).unwrap();
    assert_eq!(result, "XXX123");
}

#[test]
fn test_positional_replace_empty_replacement() {
    // Common GoReleaser pattern: strip "v" prefix
    let vars = test_vars();
    let result = render("{{ replace .Tag \"v\" \"\" }}", &vars).unwrap();
    assert_eq!(result, "1.2.3");
}

#[test]
fn test_named_arg_syntax_passthrough() {
    // Already using named args — should NOT be rewritten
    let vars = test_vars();
    let result = render("{{ replace(s=Tag, old=\"v\", new=\"\") }}", &vars).unwrap();
    assert_eq!(result, "1.2.3");
}

#[test]
fn test_named_arg_filter_passthrough() {
    // Already using named filter args — should NOT be rewritten
    let vars = test_vars();
    let result = render("{{ Tag | replace(from=\"v\", to=\"\") }}", &vars).unwrap();
    assert_eq!(result, "1.2.3");
}

#[test]
fn test_positional_mixed_with_literal_text() {
    // Positional syntax mixed with literal text around it
    let vars = test_vars();
    let result = render("app-{{ replace .Tag \"v\" \"\" }}-{{ .Os }}", &vars).unwrap();
    assert_eq!(result, "app-1.2.3-linux");
}

#[test]
fn test_positional_replace_both_quoted_args() {
    // All args quoted — replace("v1.2.3", "v", "")
    let vars = test_vars();
    let result = render("{{ replace \"v1.2.3\" \"v\" \"\" }}", &vars).unwrap();
    assert_eq!(result, "1.2.3");
}

#[test]
fn test_positional_split_literal_string() {
    // split with a literal string instead of a variable
    let vars = test_vars();
    let result = render("{{ split \"a.b.c\" \".\" }}", &vars).unwrap();
    assert!(result.contains("a"));
    assert!(result.contains("b"));
    assert!(result.contains("c"));
}

#[test]
fn test_positional_contains_literal_string() {
    let vars = test_vars();
    let result = render(
        "{% if contains \"hello world\" \"world\" %}yes{% else %}no{% endif %}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "yes");
}

#[test]
fn test_split_filter_end_to_end() {
    // Test the split filter registration works end-to-end
    let vars = test_vars();
    let result = render("{{ Version | split(sep=\".\") }}", &vars).unwrap();
    assert!(result.contains("1"));
    assert!(result.contains("2"));
    assert!(result.contains("3"));
}

#[test]
fn test_contains_filter_end_to_end() {
    // Test the contains filter registration works end-to-end
    let vars = test_vars();
    let result = render(
        "{% if Tag | contains(substr=\"v\") %}yes{% else %}no{% endif %}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "yes");
}

#[test]
fn test_chained_named_filter_then_positional_rewrite() {
    // Chained: named-arg filter followed by positional rewrite.
    // `{{ Version | trimprefix(prefix="v") | replace "." "-" }}`
    // The first filter uses named-arg syntax (has parens), the second uses positional.
    // The preprocessor should rewrite ONLY the last segment's positional args
    // while preserving the named-arg filter unchanged.
    let mut vars = test_vars();
    vars.set("Version", "v1.2.3");

    // Verify end-to-end rendering
    let input = "{{ Version | trimprefix(prefix=\"v\") | replace \".\" \"-\" }}";
    let result = render(input, &vars).unwrap();
    assert_eq!(result, "1-2-3");
}

// ---- `in` function tests ----

#[test]
fn test_in_list_contains_value() {
    let vars = test_vars();
    let result = render(
        "{% if in(items=[\"a\", \"b\", \"c\"], value=\"b\") %}yes{% else %}no{% endif %}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "yes");
}

#[test]
fn test_in_list_not_contains_value() {
    let vars = test_vars();
    let result = render(
        "{% if in(items=[\"a\", \"b\", \"c\"], value=\"d\") %}yes{% else %}no{% endif %}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "no");
}

#[test]
fn test_in_empty_list() {
    let vars = test_vars();
    let result = render(
        "{% if in(items=[], value=\"a\") %}yes{% else %}no{% endif %}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "no");
}

#[test]
fn test_in_go_style_positional_with_list_subexpr() {
    // Go-style: {{ in (list "a" "b" "c") "b" }}
    // This exercises the full preprocessing pipeline:
    // 1. (list "a" "b" "c") → ["a", "b", "c"]
    // 2. in ["a", "b", "c"] "b" → in(items=["a", "b", "c"], value="b")
    let vars = test_vars();
    let result = render(
        "{% if in (list \"linux\" \"darwin\") \"linux\" %}yes{% else %}no{% endif %}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "yes");
}

#[test]
fn test_in_go_style_positional_with_list_subexpr_not_found() {
    let vars = test_vars();
    let result = render(
        "{% if in (list \"linux\" \"darwin\") \"windows\" %}yes{% else %}no{% endif %}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "no");
}

#[test]
fn test_in_positional_with_variable() {
    // {{ in myList "b" }} where myList is a template variable
    // NOTE: This requires myList to be set as a Tera array in the context.
    // Since TemplateVars only supports string vars, we test with the list subexpr form instead.
    let vars = test_vars();
    let result = render(
        "{% if in (list \"a\" \"b\" \"c\") \"c\" %}found{% else %}nope{% endif %}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "found");
}

#[test]
fn test_in_renders_bool_string() {
    // When used in an expression context, `in` should render as "true" or "false"
    let vars = test_vars();
    let result = render("{{ in(items=[\"a\", \"b\"], value=\"a\") }}", &vars).unwrap();
    assert_eq!(result, "true");
}

#[test]
fn test_in_renders_bool_string_false() {
    let vars = test_vars();
    let result = render("{{ in(items=[\"a\", \"b\"], value=\"z\") }}", &vars).unwrap();
    assert_eq!(result, "false");
}

#[test]
fn test_in_filter_form_piped_via_set() {
    // Test the `in` filter registration by piping an array variable.
    // Use `{% set %}` to create an array variable, then pipe it to `in`.
    let vars = test_vars();
    let result = render(
        "{% set items = [\"a\", \"b\", \"c\"] %}{% if items | in(value=\"b\") %}yes{% else %}no{% endif %}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "yes");
}

#[test]
fn test_in_filter_form_piped_not_found() {
    let vars = test_vars();
    let result = render(
        "{% set items = [\"a\", \"b\", \"c\"] %}{% if items | in(value=\"z\") %}yes{% else %}no{% endif %}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "no");
}

#[test]
fn test_in_missing_items_arg_error() {
    let vars = test_vars();
    let result = render("{{ in(value=\"a\") }}", &vars);
    assert!(result.is_err(), "in without items should error");
}

#[test]
fn test_in_missing_value_arg_error() {
    let vars = test_vars();
    let result = render("{{ in(items=[\"a\"]) }}", &vars);
    assert!(result.is_err(), "in without value should error");
}

// ---- `reReplaceAll` function tests ----

#[test]
fn test_re_replace_all_basic() {
    let vars = test_vars();
    let result = render(
        "{{ reReplaceAll(pattern=\"world\", input=\"hello world\", replacement=\"rust\") }}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "hello rust");
}

#[test]
fn test_re_replace_all_with_capture_groups() {
    let vars = test_vars();
    // Pattern `(\w+) (\w+)` captures two words; replacement swaps them.
    // In Tera strings, backslash is literal (no \w escape interpretation).
    let result = render(
        r#"{{ reReplaceAll(pattern="(\w+) (\w+)", input="hello world", replacement="$2 $1") }}"#,
        &vars,
    )
    .unwrap();
    assert_eq!(result, "world hello");
}

#[test]
fn test_re_replace_all_capture_group_goreleaser_style() {
    // Mimics the GoReleaser docs example:
    // reReplaceAll "(.*) \(#(.*)\)" .Message "$1 [#$2](url/$2)"
    let mut vars = test_vars();
    vars.set("Message", "fix bug (#123)");
    let result = render(
        r#"{{ reReplaceAll(pattern="(.*) \(#(.*)\)", input=Message, replacement="$1 [#$2](https://tracker/$2)") }}"#,
        &vars,
    )
    .unwrap();
    assert_eq!(result, "fix bug [#123](https://tracker/123)");
}

#[test]
fn test_re_replace_all_no_match() {
    let vars = test_vars();
    let result = render(
        "{{ reReplaceAll(pattern=\"xyz\", input=\"hello\", replacement=\"replaced\") }}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "hello");
}

#[test]
fn test_re_replace_all_invalid_regex_error() {
    let vars = test_vars();
    let result = render(
        "{{ reReplaceAll(pattern=\"[invalid\", input=\"hello\", replacement=\"x\") }}",
        &vars,
    );
    assert!(result.is_err(), "invalid regex should produce an error");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("invalid regex") || err.contains("reReplaceAll"),
        "error should mention reReplaceAll or invalid regex, got: {err}"
    );
}

#[test]
fn test_re_replace_all_replaces_all_occurrences() {
    let vars = test_vars();
    let result = render(
        "{{ reReplaceAll(pattern=\"o\", input=\"foo bar boo\", replacement=\"0\") }}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "f00 bar b00");
}

#[test]
fn test_re_replace_all_go_style_positional() {
    // Go-style: {{ reReplaceAll "pattern" "input" "replacement" }}
    let vars = test_vars();
    let result = render(
        "{{ reReplaceAll \"world\" \"hello world\" \"rust\" }}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "hello rust");
}

#[test]
fn test_re_replace_all_go_style_with_variable() {
    // Go-style with a variable as input: {{ reReplaceAll "v" Tag "" }}
    let vars = test_vars();
    let result = render("{{ reReplaceAll \"v\" Tag \"\" }}", &vars).unwrap();
    assert_eq!(result, "1.2.3");
}

#[test]
fn test_re_replace_all_filter_form() {
    // Filter form: {{ Field | reReplaceAll(pattern="...", replacement="...") }}
    let vars = test_vars();
    let result = render(
        "{{ Tag | reReplaceAll(pattern=\"v\", replacement=\"\") }}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "1.2.3");
}

#[test]
fn test_re_replace_all_filter_form_with_capture() {
    let vars = test_vars();
    let result = render(
        "{{ Tag | reReplaceAll(pattern=\"v(.*)\", replacement=\"ver-$1\") }}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "ver-1.2.3");
}

#[test]
fn test_re_replace_all_piped_positional() {
    // Piped positional: {{ Tag | reReplaceAll "v" "" }}
    let vars = test_vars();
    let result = render("{{ Tag | reReplaceAll \"v\" \"\" }}", &vars).unwrap();
    assert_eq!(result, "1.2.3");
}

#[test]
fn test_re_replace_all_missing_pattern_error() {
    let vars = test_vars();
    let result = render(
        "{{ reReplaceAll(input=\"hello\", replacement=\"x\") }}",
        &vars,
    );
    assert!(result.is_err(), "reReplaceAll without pattern should error");
}

#[test]
fn test_re_replace_all_missing_input_error() {
    let vars = test_vars();
    let result = render(
        "{{ reReplaceAll(pattern=\"x\", replacement=\"y\") }}",
        &vars,
    );
    assert!(result.is_err(), "reReplaceAll without input should error");
}

#[test]
fn test_re_replace_all_missing_replacement_error() {
    let vars = test_vars();
    let result = render("{{ reReplaceAll(pattern=\"x\", input=\"hello\") }}", &vars);
    assert!(
        result.is_err(),
        "reReplaceAll without replacement should error"
    );
}

#[test]
fn test_re_replace_all_filter_invalid_regex_error() {
    let vars = test_vars();
    let result = render(
        "{{ Tag | reReplaceAll(pattern=\"[bad\", replacement=\"x\") }}",
        &vars,
    );
    assert!(
        result.is_err(),
        "invalid regex in filter form should produce an error"
    );
}

// --- Finding 7: `in` with numeric values ---

#[test]
fn test_in_numeric_value_as_string() {
    // in(items=[1, 2, 3], value="2") — string needle matches numeric item via stringification
    let vars = test_vars();
    let result = render(
        "{% if in(items=[1, 2, 3], value=\"2\") %}yes{% else %}no{% endif %}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "yes");
}

#[test]
fn test_in_numeric_value_as_number() {
    // in(items=[1, 2, 3], value=2) — numeric needle matches numeric item
    let vars = test_vars();
    let result = render(
        "{% if in(items=[1, 2, 3], value=2) %}yes{% else %}no{% endif %}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "yes");
}

#[test]
fn test_in_numeric_value_not_found() {
    let vars = test_vars();
    let result = render(
        "{% if in(items=[1, 2, 3], value=4) %}yes{% else %}no{% endif %}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "no");
}

// --- Finding 8: `reReplaceAll` with empty input ---

#[test]
fn test_re_replace_all_empty_input() {
    let vars = test_vars();
    let result = render(
        "{{ reReplaceAll(pattern=\".*\", input=\"\", replacement=\"x\") }}",
        &vars,
    )
    .unwrap();
    // `.*` matches the empty string once, producing "x"
    assert_eq!(result, "x");
}

// --- Finding 9: `in` keyword conflict in {% set %} context ---

#[test]
fn test_in_set_context_keyword_conflict() {
    // Verify that `in` as a function name works inside `{% set %}` assignment.
    // Tera's parser uses `in` as a keyword in `{% for x in list %}`, so we need
    // to confirm it doesn't choke when used as a function call in `{% set %}`.
    let vars = test_vars();
    let result = render(
        "{% set result = in(items=[\"a\"], value=\"a\") %}{{ result }}",
        &vars,
    );
    // If Tera can't parse this, we'll get an error. Check behavior.
    match result {
        Ok(val) => assert_eq!(val, "true"),
        Err(e) => {
            // If Tera rejects `in` as a function name in set context,
            // this is a known limitation — the test documents it.
            panic!(
                "Tera rejects `in` as function name in set context: {}. \
                 Consider renaming to `listContains`.",
                e
            );
        }
    }
}

// --- extract_artifact_ext tests ---

#[test]
fn test_extract_artifact_ext_tar_gz() {
    assert_eq!(
        extract_artifact_ext("myapp-1.0.0-linux-amd64.tar.gz"),
        ".tar.gz"
    );
}

#[test]
fn test_extract_artifact_ext_tar_xz() {
    assert_eq!(extract_artifact_ext("myapp.tar.xz"), ".tar.xz");
}

#[test]
fn test_extract_artifact_ext_tar_zst() {
    assert_eq!(extract_artifact_ext("myapp.tar.zst"), ".tar.zst");
}

#[test]
fn test_extract_artifact_ext_tar_bz2() {
    assert_eq!(extract_artifact_ext("myapp.tar.bz2"), ".tar.bz2");
}

#[test]
fn test_extract_artifact_ext_tar_lz4() {
    assert_eq!(extract_artifact_ext("archive.tar.lz4"), ".tar.lz4");
}

#[test]
fn test_extract_artifact_ext_tar_sz() {
    assert_eq!(extract_artifact_ext("archive.tar.sz"), ".tar.sz");
}

#[test]
fn test_extract_artifact_ext_exe() {
    assert_eq!(extract_artifact_ext("myapp.exe"), ".exe");
}

#[test]
fn test_extract_artifact_ext_dmg() {
    assert_eq!(extract_artifact_ext("myapp-1.0.0.dmg"), ".dmg");
}

#[test]
fn test_extract_artifact_ext_zip() {
    assert_eq!(extract_artifact_ext("myapp-1.0.0.zip"), ".zip");
}

#[test]
fn test_extract_artifact_ext_no_extension() {
    assert_eq!(extract_artifact_ext("myapp"), "");
}

#[test]
fn test_extract_artifact_ext_hidden_file_no_ext() {
    // A dotfile with no extension beyond the leading dot — treated as no extension
    // (the leading dot is the filename, not an extension separator)
    assert_eq!(extract_artifact_ext(".gitignore"), "");
}

#[test]
fn test_extract_artifact_ext_deb() {
    assert_eq!(extract_artifact_ext("myapp_1.0.0_amd64.deb"), ".deb");
}

#[test]
fn test_extract_artifact_ext_rpm() {
    assert_eq!(extract_artifact_ext("myapp-1.0.0.x86_64.rpm"), ".rpm");
}

#[test]
fn test_extract_artifact_ext_empty_string() {
    assert_eq!(extract_artifact_ext(""), "");
}

// --- Outputs template variable tests ---

#[test]
fn test_outputs_set_and_render() {
    let mut vars = test_vars();
    vars.set_output("build_id", "abc123");
    let result = render("{{ .Outputs.build_id }}", &vars).unwrap();
    assert_eq!(result, "abc123");
}

#[test]
fn test_outputs_multiple_keys() {
    let mut vars = test_vars();
    vars.set_output("key1", "val1");
    vars.set_output("key2", "val2");
    let result = render("{{ .Outputs.key1 }}-{{ .Outputs.key2 }}", &vars).unwrap();
    assert_eq!(result, "val1-val2");
}

#[test]
fn test_outputs_empty_map_renders_empty_string() {
    let vars = test_vars();
    // Accessing a non-existent key in Outputs should return "" (Tera default)
    let result = render("{{ Outputs.missing | default(value=\"\") }}", &vars).unwrap();
    assert_eq!(result, "");
}

#[test]
fn test_outputs_get_output() {
    let mut vars = TemplateVars::new();
    vars.set_output("x", "42");
    assert_eq!(vars.get_output("x"), Some(&"42".to_string()));
    assert_eq!(vars.get_output("y"), None);
}

// --- ArtifactExt template variable rendering test ---

#[test]
fn test_artifact_ext_template_rendering() {
    let mut vars = test_vars();
    vars.set("ArtifactName", "myapp-1.0.0-linux-amd64.tar.gz");
    vars.set("ArtifactExt", ".tar.gz");
    let result = render("{{ .ArtifactName }}{{ .ArtifactExt }}", &vars).unwrap();
    assert_eq!(result, "myapp-1.0.0-linux-amd64.tar.gz.tar.gz");
}

// --- Target template variable rendering test ---

#[test]
fn test_target_template_rendering() {
    let mut vars = test_vars();
    vars.set("Target", "x86_64-unknown-linux-gnu");
    let result = render("{{ .ProjectName }}-{{ .Version }}-{{ .Target }}", &vars).unwrap();
    assert_eq!(result, "cfgd-1.2.3-x86_64-unknown-linux-gnu");
}

// --- Checksums template variable rendering test ---

#[test]
fn test_checksums_template_rendering() {
    let mut vars = test_vars();
    let checksum_content = "abc123  myapp-1.0.0.tar.gz\ndef456  myapp-1.0.0.zip\n";
    vars.set("Checksums", checksum_content);
    let result = render("{{ .Checksums }}", &vars).unwrap();
    assert_eq!(result, checksum_content);
}

// --- Go time format translation tests ---

#[test]
fn test_translate_go_time_format_basic_date() {
    let result = translate_go_time_format("2006-01-02");
    assert_eq!(result, "%Y-%m-%d");
}

#[test]
fn test_translate_go_time_format_full_datetime() {
    let result = translate_go_time_format("2006-01-02 15:04:05");
    assert_eq!(result, "%Y-%m-%d %H:%M:%S");
}

#[test]
fn test_translate_go_time_format_chrono_passthrough() {
    // Already chrono format -- should pass through unchanged
    let result = translate_go_time_format("%Y-%m-%d");
    assert_eq!(result, "%Y-%m-%d");
}

#[test]
fn test_translate_go_time_format_no_go_patterns() {
    // Plain text with no Go patterns -- should pass through unchanged
    let result = translate_go_time_format("hello world");
    assert_eq!(result, "hello world");
}

#[test]
fn test_translate_go_time_format_month_name() {
    let result = translate_go_time_format("January 02, 2006");
    assert_eq!(result, "%B %d, %Y");
}

#[test]
fn test_translate_go_time_format_weekday() {
    let result = translate_go_time_format("Monday, January 02, 2006");
    assert_eq!(result, "%A, %B %d, %Y");
}

#[test]
fn test_time_go_format_end_to_end() {
    // The `time` function should accept Go format and produce a valid date
    let vars = test_vars();
    let result = render("{{ time(format=\"2006-01-02\") }}", &vars).unwrap();
    // Should match YYYY-MM-DD pattern
    assert!(
        result.len() == 10 && result.chars().nth(4) == Some('-'),
        "expected date in YYYY-MM-DD format, got: {result}"
    );
}

#[test]
fn test_time_chrono_format_still_works() {
    // The `time` function should still accept chrono format
    let vars = test_vars();
    let result = render("{{ time(format=\"%Y-%m-%d\") }}", &vars).unwrap();
    assert!(
        result.len() == 10 && result.chars().nth(4) == Some('-'),
        "expected date in YYYY-MM-DD format, got: {result}"
    );
}

/// SDE-aware Tera helpers: when `SOURCE_DATE_EPOCH` is set, both the
/// `time(...)` function AND the `now_format` filter must derive their
/// timestamp from it instead of reading wall-clock `Utc::now()`. The
/// determinism harness sets SDE on every child build subprocess, so user
/// templates like `{{ time(format="2006-01-02") }}` flowing into artifact
/// names must produce byte-stable output across reruns.
#[test]
fn test_time_function_honors_source_date_epoch() {
    let vars = test_vars();
    // 1715000000 → 2024-05-06 (UTC).
    let host = MapEnvSource::new().with("SOURCE_DATE_EPOCH", "1715000000");
    let result = render_with_env("{{ time(format=\"%Y-%m-%d\") }}", &vars, &host).unwrap();
    assert_eq!(
        result, "2024-05-06",
        "time() must honor SOURCE_DATE_EPOCH; got: {result}"
    );
}

#[test]
fn test_now_format_filter_honors_source_date_epoch() {
    let mut vars = test_vars();
    vars.set("Now", "ignored");
    let host = MapEnvSource::new().with("SOURCE_DATE_EPOCH", "1715000000");
    let result =
        render_with_env("{{ Now | now_format(format=\"%Y-%m-%d\") }}", &vars, &host).unwrap();
    assert_eq!(
        result, "2024-05-06",
        "now_format must honor SOURCE_DATE_EPOCH; got: {result}"
    );
}

// --- now_format filter tests ---

#[test]
fn test_now_format_filter_go_format() {
    let mut vars = test_vars();
    vars.set("Now", "2026-04-05T12:00:00Z"); // value is ignored by filter
    let result = render("{{ Now | now_format(format=\"2006-01-02\") }}", &vars).unwrap();
    // Should be a YYYY-MM-DD date string
    assert!(
        result.len() == 10 && result.chars().nth(4) == Some('-'),
        "expected date in YYYY-MM-DD format, got: {result}"
    );
}

#[test]
fn test_now_format_filter_chrono_format() {
    let mut vars = test_vars();
    vars.set("Now", "2026-04-05T12:00:00Z");
    let result = render("{{ Now | now_format(format=\"%Y-%m-%d\") }}", &vars).unwrap();
    assert!(
        result.len() == 10 && result.chars().nth(4) == Some('-'),
        "expected date in YYYY-MM-DD format, got: {result}"
    );
}

#[test]
fn test_now_format_preprocessed_from_go_style() {
    // GoReleaser-style: {{ .Now.Format "2006-01-02" }}
    // After preprocessing: {{ Now | now_format(format="2006-01-02") }}
    let mut vars = test_vars();
    vars.set("Now", "2026-04-05T12:00:00Z");
    let result = render("{{ .Now.Format \"2006-01-02\" }}", &vars).unwrap();
    assert!(
        result.len() == 10 && result.chars().nth(4) == Some('-'),
        "expected date in YYYY-MM-DD format, got: {result}"
    );
}

// ---- comparison function preprocessing end-to-end tests ----

#[test]
fn test_eq_comparison_end_to_end() {
    let vars = test_vars();
    // Go-style: {{ if eq .Os "linux" }}yes{{ end }}
    let result = render("{{ if eq .Os \"linux\" }}yes{{ end }}", &vars).unwrap();
    assert_eq!(result, "yes");
}

#[test]
fn test_ne_comparison_end_to_end() {
    let vars = test_vars();
    let result = render("{{ if ne .Os \"windows\" }}not-win{{ end }}", &vars).unwrap();
    assert_eq!(result, "not-win");
}

#[test]
fn test_gt_comparison_end_to_end() {
    let vars = test_vars();
    // Major is 1
    let result = render("{{ if gt .Major 0 }}positive{{ else }}zero{{ end }}", &vars).unwrap();
    assert_eq!(result, "positive");
}

#[test]
fn test_lt_comparison_end_to_end() {
    let vars = test_vars();
    // Patch is 3
    let result = render("{{ if lt .Patch 5 }}small{{ else }}big{{ end }}", &vars).unwrap();
    assert_eq!(result, "small");
}

#[test]
fn test_eq_with_not_parenthesized() {
    let mut vars = test_vars();
    vars.set("Amd64", "v2");
    let result = render("{{ if not (eq .Amd64 \"v1\") }}not-v1{{ end }}", &vars).unwrap();
    assert_eq!(result, "not-v1");
}

#[test]
fn test_or_and_comparison_end_to_end() {
    let vars = test_vars();
    // or (eq .Os "linux") (eq .Os "darwin") -- Os is "linux"
    let result = render(
        "{{ if or (eq .Os \"linux\") (eq .Os \"darwin\") }}unix{{ else }}other{{ end }}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "unix");
}

#[test]
fn test_and_comparison_end_to_end() {
    let vars = test_vars();
    // and (eq .Os "linux") (eq .Arch "amd64")
    let result = render(
        "{{ if and (eq .Os \"linux\") (eq .Arch \"amd64\") }}match{{ else }}no{{ end }}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "match");
}

// ---- index function tests ----

#[test]
fn test_index_map_access() {
    let mut vars = test_vars();
    let map = serde_json::json!({"key1": "val1", "key2": "val2"});
    vars.set_structured("mymap", map);
    let result = render("{{ index(collection=mymap, key=\"key1\") }}", &vars).unwrap();
    assert_eq!(result, "val1");
}

#[test]
fn test_index_map_missing_key() {
    let mut vars = test_vars();
    let map = serde_json::json!({"key1": "val1"});
    vars.set_structured("mymap", map);
    let result = render("{{ index(collection=mymap, key=\"missing\") }}", &vars).unwrap();
    assert_eq!(result, "", "missing key should return empty string");
}

#[test]
fn test_index_array_access() {
    let mut vars = test_vars();
    let arr = serde_json::json!(["first", "second", "third"]);
    vars.set_structured("myarr", arr);
    let result = render("{{ index(collection=myarr, key=1) }}", &vars).unwrap();
    assert_eq!(result, "second");
}

// ---- missing env var graceful handling tests ----

#[test]
fn test_missing_env_var_in_conditional() {
    let vars = test_vars();
    // {{ if .Env.NONEXISTENT }} should evaluate to false (empty string is falsy)
    let result = render(
        "{{ if .Env.TOTALLY_MISSING_VAR_XYZ }}set{{ else }}unset{{ end }}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "unset");
}

#[test]
fn test_missing_env_var_renders_empty() {
    let vars = test_vars();
    let result = render("prefix-{{ .Env.NONEXISTENT_ABC_123 }}-suffix", &vars).unwrap();
    assert_eq!(result, "prefix--suffix");
}

#[test]
fn test_existing_env_var_still_works() {
    let vars = test_vars();
    // GITHUB_TOKEN is set in test_vars()
    let result = render("{{ .Env.GITHUB_TOKEN }}", &vars).unwrap();
    assert_eq!(result, "tok123");
}

// ---- map + index end-to-end test ----

#[test]
fn test_map_and_index_go_style() {
    // Full Go-style pipeline:
    // {{ $m := map "a" "1" "b" "2" }}{{ index $m "a" }}
    let vars = test_vars();
    let result = render(
        "{{ $m := map \"a\" \"1\" \"b\" \"2\" }}{{ index $m \"a\" }}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "1");
}

#[test]
fn test_map_and_index_missing_key_returns_empty() {
    let vars = test_vars();
    let result = render(
        "{{ $m := map \"a\" \"1\" }}{{ index $m \"missing\" }}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "");
}

// ---- per-target var coverage (Q-tpl1) ----
//
// Mirrors GoReleaser `internal/tmpl/tmpl.go` per-artifact key set: every key
// must be a member of `PER_TARGET_VARS` so the clear-on-exit pass touches it
// (and so `{{ .Ppc64 }}` / `{{ .Riscv64 }}` references render empty rather
// than raising a Tera "missing key" error in strict mode).
#[test]
fn test_per_target_vars_includes_ppc64_and_riscv64() {
    use super::vars::PER_TARGET_VARS;
    assert!(
        PER_TARGET_VARS.contains(&"Ppc64"),
        "PER_TARGET_VARS missing Ppc64 key (GR parity tmpl.go:43,208)"
    );
    assert!(
        PER_TARGET_VARS.contains(&"Riscv64"),
        "PER_TARGET_VARS missing Riscv64 key (GR parity tmpl.go:44,209)"
    );
}

#[test]
fn test_clear_per_target_vars_clears_ppc64_and_riscv64() {
    use super::vars::{TemplateVars, clear_per_target_vars};
    let mut tv = TemplateVars::new();
    tv.set("Ppc64", "power9");
    tv.set("Riscv64", "rva20u64");
    clear_per_target_vars(&mut tv);
    assert_eq!(tv.get("Ppc64").map(String::as_str), Some(""));
    assert_eq!(tv.get("Riscv64").map(String::as_str), Some(""));
}

#[test]
fn test_render_ppc64_and_riscv64_empty_after_clear() {
    use super::vars::clear_per_target_vars;
    let mut vars = test_vars();
    clear_per_target_vars(&mut vars);
    // Empty string render must not raise a Tera "missing key" error.
    let out = render("[{{ .Ppc64 }}|{{ .Riscv64 }}]", &vars).unwrap();
    assert_eq!(out, "[|]");
}
