use std::collections::HashMap;
use tera::Value;

use super::base_tera::translate_go_time_format;
use super::render::{extract_artifact_ext, render, render_with_env};
use super::vars::{TemplateVars, find_stale_typed_compare};
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
    // Regression guard: the changelog renderer publishes per-entry
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
    // Missing env vars resolve to an empty string.
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
// Hash functions read file contents, so tests use temp files.

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
// returning "0.0.1" / "0.1.0" / "1.0.0". A non-semver input panics on parse.
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

// ---- ruby_escape tests ----

#[test]
fn test_ruby_escape_quote_and_backslash() {
    let mut vars = test_vars();
    // Backslash must be escaped before the quote so the quote's escape
    // backslash is not itself doubled. Input: the "best" \tool
    vars.set("Input", "the \"best\" \\tool");
    let result = render("{{ Input | ruby_escape }}", &vars).unwrap();
    // Expected: the \"best\" \\tool
    assert_eq!(result, "the \\\"best\\\" \\\\tool");
}

#[test]
fn test_ruby_escape_plain_string_unchanged() {
    let mut vars = test_vars();
    vars.set("Input", "no special chars here");
    let result = render("{{ Input | ruby_escape }}", &vars).unwrap();
    assert_eq!(result, "no special chars here");
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
// when the user supplies an invalid regex. A POSIX regex compiler
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

#[test]
fn test_index_or_default_function_via_render_key_present() {
    // Drive the *registered* `indexOrDefault` closure end-to-end through the
    // template engine (the sibling tests above reimplement the logic inline and
    // never execute the wired-up function). A structured map var is the only way
    // to hand Tera an object argument.
    let mut vars = test_vars();
    vars.set_structured(
        "Labels",
        serde_json::json!({ "env": "prod", "tier": "web" }),
    );
    let out = render(
        "{{ indexOrDefault(map=Labels, key=\"env\", default=\"unknown\") }}",
        &vars,
    )
    .unwrap();
    assert_eq!(out, "prod");
}

#[test]
fn test_index_or_default_function_via_render_key_missing_uses_default() {
    let mut vars = test_vars();
    vars.set_structured("Labels", serde_json::json!({ "env": "prod" }));
    let out = render(
        "{{ indexOrDefault(map=Labels, key=\"absent\", default=\"unknown\") }}",
        &vars,
    )
    .unwrap();
    assert_eq!(out, "unknown");
}

#[test]
fn test_index_or_default_function_missing_map_arg_errors() {
    // The `map`-argument guard inside the registered closure.
    let vars = test_vars();
    let result = render("{{ indexOrDefault(key=\"x\", default=\"d\") }}", &vars);
    assert!(
        result.is_err(),
        "indexOrDefault without a `map` argument must error"
    );
}

// ---- filter / reverseFilter pipe-form line filtering ----

#[test]
fn test_filter_pipe_keeps_matching_lines() {
    // The successful filtering branch of the `filter` pipe closure (a multiline
    // string split into lines, kept when the regex matches, rejoined). The
    // existing pipe tests only exercise the invalid-regex error path.
    let mut vars = test_vars();
    vars.set("Notes", "feat: a\nfix: b\nfeat: c\nchore: d");
    let out = render("{{ Notes | filter(regexp=\"^feat\") }}", &vars).unwrap();
    assert_eq!(out, "feat: a\nfeat: c");
}

#[test]
fn test_reverse_filter_pipe_drops_matching_lines() {
    // The successful filtering branch of the `reverseFilter` pipe closure:
    // keep the lines that do NOT match the regex.
    let mut vars = test_vars();
    vars.set("Notes", "feat: a\nfix: b\nfeat: c\nchore: d");
    let out = render("{{ Notes | reverseFilter(regexp=\"^feat\") }}", &vars).unwrap();
    assert_eq!(out, "fix: b\nchore: d");
}

#[test]
fn test_now_format_filter_missing_format_arg_errors() {
    // The `format`-argument guard inside the registered `now_format` filter.
    let vars = test_vars();
    let result = render("{{ Now | now_format() }}", &vars);
    assert!(
        result.is_err(),
        "now_format without a `format` argument must error"
    );
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
    // Common pattern: strip "v" prefix
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
    // Mimics the docs example:
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
    // Date formatting: {{ .Now.Format "2006-01-02" }}
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
// The per-artifact key set: every key
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

#[test]
fn test_clear_per_artifact_vars_clears_both_target_and_artifact_keys() {
    // `clear_per_artifact_vars` must clear the per-target keys (via the inner
    // `clear_per_target_vars` call) AND the per-artifact keys, so a stale
    // ArtifactName/Os from one loop iteration cannot leak into a later stage.
    use super::vars::{PER_ARTIFACT_VARS, PER_TARGET_VARS, TemplateVars, clear_per_artifact_vars};
    let mut tv = TemplateVars::new();
    tv.set("Os", "linux");
    tv.set("Arch", "amd64");
    tv.set("ArtifactName", "myapp-1.0.0.tar.gz");
    tv.set("ArtifactExt", ".tar.gz");
    tv.set("ArtifactID", "myapp-archive");
    clear_per_artifact_vars(&mut tv);
    for key in PER_TARGET_VARS.iter().chain(PER_ARTIFACT_VARS.iter()) {
        assert_eq!(
            tv.get(key).map(String::as_str),
            Some(""),
            "per-artifact clear must blank `{key}`"
        );
    }
}

// --- Go `slice` / `printf` / `print` / `println` builtins ---

#[test]
fn test_slice_string_end_exclusive() {
    let vars = test_vars();
    // Go slice(s, 0, 7) is end-exclusive → first 7 chars "abcdefg".
    let result = render("{{ slice \"abcdefghij\" 0 7 }}", &vars).unwrap();
    assert_eq!(result, "abcdefg");
}

#[test]
fn test_slice_start_only() {
    let vars = test_vars();
    let result = render("{{ slice \"abcdefghij\" 3 }}", &vars).unwrap();
    assert_eq!(result, "defghij");
}

#[test]
fn test_slice_short_commit() {
    let vars = test_vars();
    // ShortCommit = "abc1234" (7 chars); slice 0 4 → "abc1".
    let result = render("{{ slice .ShortCommit 0 4 }}", &vars).unwrap();
    assert_eq!(result, "abc1");
}

#[test]
fn test_slice_out_of_range_clamps() {
    let vars = test_vars();
    // end beyond length clamps to the full string (no panic).
    let result = render("{{ slice \"hi\" 0 100 }}", &vars).unwrap();
    assert_eq!(result, "hi");
}

#[test]
fn test_slice_char_boundary_safe() {
    let mut vars = test_vars();
    // `é` (U+00E9) is two UTF-8 bytes; slicing 0..2 must yield two *chars*
    // ("hé"), not split a byte. Feed via a structured var to keep the source
    // file ASCII-only.
    let word = format!("h{}llo", '\u{00E9}');
    vars.set_structured("Word", Value::String(word));
    let result = render("{{ slice Word 0 2 }}", &vars).unwrap();
    assert_eq!(result, format!("h{}", '\u{00E9}'));
}

#[test]
fn test_slice_piped_form() {
    let vars = test_vars();
    let result = render("{{ \"abcdef\" | slice(start=1, end=4) }}", &vars).unwrap();
    assert_eq!(result, "bcd");
}

#[test]
fn test_printf_zero_pad() {
    let vars = test_vars();
    let result = render("{{ printf \"%04d\" 7 }}", &vars).unwrap();
    assert_eq!(result, "0007");
}

#[test]
fn test_printf_left_align_string() {
    let vars = test_vars();
    let result = render("{{ printf \"%-5s|\" \"hi\" }}", &vars).unwrap();
    assert_eq!(result, "hi   |");
}

#[test]
fn test_printf_float_precision() {
    let vars = test_vars();
    let result = render("{{ printf \"%.2f\" 3.14159 }}", &vars).unwrap();
    assert_eq!(result, "3.14");
}

#[test]
fn test_printf_plus_sign() {
    let vars = test_vars();
    let result = render("{{ printf \"%+d\" 7 }}", &vars).unwrap();
    assert_eq!(result, "+7");
}

#[test]
fn test_printf_negative_int() {
    let mut vars = test_vars();
    // A bare `-7` literal can't survive Tera's expression parser (it parses as
    // subtraction), so feed the negative value through a structured var.
    vars.set_structured("Neg", Value::Number((-7i64).into()));
    let result = render("{{ printf \"%05d\" Neg }}", &vars).unwrap();
    assert_eq!(result, "-0007");
}

#[test]
fn test_printf_hex() {
    let vars = test_vars();
    assert_eq!(render("{{ printf \"%x\" 255 }}", &vars).unwrap(), "ff");
    assert_eq!(render("{{ printf \"%X\" 255 }}", &vars).unwrap(), "FF");
    assert_eq!(render("{{ printf \"%#x\" 255 }}", &vars).unwrap(), "0xff");
}

#[test]
fn test_printf_octal_binary() {
    let vars = test_vars();
    assert_eq!(render("{{ printf \"%o\" 8 }}", &vars).unwrap(), "10");
    assert_eq!(render("{{ printf \"%b\" 5 }}", &vars).unwrap(), "101");
}

#[test]
fn test_printf_string_and_v() {
    let vars = test_vars();
    assert_eq!(render("{{ printf \"%s\" \"hi\" }}", &vars).unwrap(), "hi");
    assert_eq!(render("{{ printf \"%v\" 42 }}", &vars).unwrap(), "42");
}

#[test]
fn test_printf_quote_verb() {
    let vars = test_vars();
    let result = render("{{ printf \"%q\" \"hi\" }}", &vars).unwrap();
    assert_eq!(result, "\"hi\"");
}

#[test]
fn test_printf_bool_verb() {
    let vars = test_vars();
    let result = render("{{ printf \"%t\" true }}", &vars).unwrap();
    assert_eq!(result, "true");
}

#[test]
fn test_printf_char_verb() {
    let vars = test_vars();
    let result = render("{{ printf \"%c\" 65 }}", &vars).unwrap();
    assert_eq!(result, "A");
}

#[test]
fn test_printf_exp_verb_go_style() {
    let vars = test_vars();
    // Go renders a signed, min-two-digit exponent: 1.23e+04 (not Rust's 1.23e4).
    let result = render("{{ printf \"%.2e\" 12345.678 }}", &vars).unwrap();
    assert_eq!(result, "1.23e+04");
}

#[test]
fn test_printf_exp_verb_negative_exponent() {
    let vars = test_vars();
    // A small magnitude → negative exponent, zero-padded to two digits.
    let result = render("{{ printf \"%.2e\" 0.0000123 }}", &vars).unwrap();
    assert_eq!(result, "1.23e-05");
}

#[test]
fn test_printf_exp_verb_three_digit_exponent() {
    let mut vars = test_vars();
    // A 3-digit exponent keeps all digits (Go: e+100), fed via a structured
    // var since `1e100` is not a parseable bare Tera literal.
    vars.set_structured(
        "Huge",
        Value::Number(serde_json::Number::from_f64(1e100).unwrap()),
    );
    let result = render("{{ printf \"%.2e\" Huge }}", &vars).unwrap();
    assert_eq!(result, "1.00e+100");
}

#[test]
fn test_printf_exp_uppercase() {
    let vars = test_vars();
    let result = render("{{ printf \"%.2E\" 12345.678 }}", &vars).unwrap();
    assert_eq!(result, "1.23E+04");
}

#[test]
fn test_printf_g_verb_plain_decimal() {
    let vars = test_vars();
    // %g of a value that renders in plain-decimal form has no exponent.
    let result = render("{{ printf \"%g\" 3.14 }}", &vars).unwrap();
    assert_eq!(result, "3.14");
}

#[test]
fn test_printf_percent_literal() {
    let vars = test_vars();
    let result = render("{{ printf \"100%%\" }}", &vars).unwrap();
    assert_eq!(result, "100%");
}

#[test]
fn test_printf_multiple_args() {
    let vars = test_vars();
    let result = render("{{ printf \"%s-%04d\" \"v\" 7 }}", &vars).unwrap();
    assert_eq!(result, "v-0007");
}

#[test]
fn test_printf_unsupported_verb_errors() {
    let vars = test_vars();
    let result = render("{{ printf \"%y\" 1 }}", &vars);
    assert!(result.is_err());
    let msg = format!("{:?}", result.unwrap_err());
    assert!(msg.contains("unsupported verb"), "got: {}", msg);
}

#[test]
fn test_print_concatenates() {
    let vars = test_vars();
    let result = render("{{ print \"a\" \"b\" }}", &vars).unwrap();
    assert_eq!(result, "ab");
}

#[test]
fn test_println_joins_with_space_and_newline() {
    let vars = test_vars();
    let result = render("{{ println \"x\" \"y\" }}", &vars).unwrap();
    assert_eq!(result, "x y\n");
}

#[test]
fn test_println_single_arg() {
    let vars = test_vars();
    let result = render("{{ println \"x\" }}", &vars).unwrap();
    assert_eq!(result, "x\n");
}

#[test]
fn test_time_positional_renders_date() {
    let vars = test_vars();
    // Pasted GoReleaser positional form must parse and render a date.
    let result = render("{{ time \"2006-01-02\" }}", &vars).unwrap();
    // Format yields YYYY-MM-DD; assert the shape rather than a fixed value.
    assert_eq!(result.len(), 10, "got: {}", result);
    assert_eq!(result.matches('-').count(), 2, "got: {}", result);
}

// --- Go printf %g exponent selection ---

#[test]
fn test_printf_g_huge_uses_exponent() {
    let mut vars = test_vars();
    vars.set_structured(
        "Huge",
        Value::Number(serde_json::Number::from_f64(1e300).unwrap()),
    );
    let result = render("{{ printf \"%g\" Huge }}", &vars).unwrap();
    assert_eq!(result, "1e+300");
}

#[test]
fn test_printf_g_tiny_uses_exponent() {
    let mut vars = test_vars();
    vars.set_structured(
        "Tiny",
        Value::Number(serde_json::Number::from_f64(1e-300).unwrap()),
    );
    let result = render("{{ printf \"%g\" Tiny }}", &vars).unwrap();
    assert_eq!(result, "1e-300");
}

#[test]
fn test_printf_g_million_uses_exponent() {
    let vars = test_vars();
    // exp == 6 >= eprec(6) → exponential per Go.
    let result = render("{{ printf \"%g\" 1000000.0 }}", &vars).unwrap();
    assert_eq!(result, "1e+06");
}

#[test]
fn test_printf_g_small_int_decimal() {
    let vars = test_vars();
    let result = render("{{ printf \"%g\" 3.14 }}", &vars).unwrap();
    assert_eq!(result, "3.14");
}

#[test]
fn test_printf_g_eight_digits_uses_exponent() {
    let vars = test_vars();
    // exp == 7 >= eprec(6) → 9.9999999e+07 (Go).
    let result = render("{{ printf \"%g\" 99999999.0 }}", &vars).unwrap();
    assert_eq!(result, "9.9999999e+07");
}

#[test]
fn test_printf_g_uppercase() {
    let mut vars = test_vars();
    vars.set_structured(
        "Huge",
        Value::Number(serde_json::Number::from_f64(1e300).unwrap()),
    );
    let result = render("{{ printf \"%G\" Huge }}", &vars).unwrap();
    assert_eq!(result, "1E+300");
}

// --- Go printf integer precision (minimum digit count) ---

#[test]
fn test_printf_int_precision() {
    let vars = test_vars();
    // Precision is the MINIMUM digit count, zero-left-padded (distinct from width).
    let result = render("{{ printf \"%.5d\" 7 }}", &vars).unwrap();
    assert_eq!(result, "00007");
}

#[test]
fn test_printf_hex_precision() {
    let vars = test_vars();
    let result = render("{{ printf \"%.3x\" 255 }}", &vars).unwrap();
    assert_eq!(result, "0ff");
}

#[test]
fn test_printf_int_precision_and_width() {
    let vars = test_vars();
    // Precision (min digits) applies before width (field padding).
    let result = render("{{ printf \"%8.5d\" 7 }}", &vars).unwrap();
    assert_eq!(result, "   00007");
}

#[test]
fn test_printf_int_precision_disables_zero_flag() {
    let vars = test_vars();
    // Go: an explicit precision disables the `0` width flag for integer verbs;
    // width is space-padded after the digits are zero-padded to precision.
    let result = render("{{ printf \"%08.5d\" 7 }}", &vars).unwrap();
    assert_eq!(result, "   00007");
}

#[test]
fn test_printf_int_precision_disables_zero_flag_negative() {
    let mut vars = test_vars();
    // Bare `-7` can't survive Tera's parser; feed via a structured var.
    vars.set_structured("Neg", Value::Number((-7i64).into()));
    let result = render("{{ printf \"%08.3d\" Neg }}", &vars).unwrap();
    assert_eq!(result, "    -007");
}

#[test]
fn test_printf_int_precision_zero_of_zero_is_empty() {
    let mut vars = test_vars();
    vars.set_structured("Z", Value::Number(0i64.into()));
    // Go: precision 0 of value 0 prints nothing.
    let result = render("{{ printf \"%.0d\" Z }}", &vars).unwrap();
    assert_eq!(result, "");
}

#[test]
fn test_printf_int_precision_zero_of_zero_width_padded() {
    let mut vars = test_vars();
    vars.set_structured("Z", Value::Number(0i64.into()));
    // Width padding applies to the (empty) result → 5 spaces.
    let result = render("{{ printf \"%5.0d\" Z }}", &vars).unwrap();
    assert_eq!(result, "     ");
}

#[test]
fn test_printf_float_zero_flag_with_precision_unaffected() {
    let vars = test_vars();
    // The precision-disables-zero rule is integer-only; floats still honor `0`.
    let result = render("{{ printf \"%08.2f\" 3.14 }}", &vars).unwrap();
    assert_eq!(result, "00003.14");
}

// --- Go printf field-size ceiling ---

#[test]
fn test_printf_width_cap_errors() {
    let vars = test_vars();
    let result = render("{{ printf \"%99999999d\" 1 }}", &vars);
    assert!(result.is_err());
    let msg = format!("{:?}", result.unwrap_err());
    assert!(
        msg.contains("printf width") && msg.contains("exceeds maximum"),
        "got: {}",
        msg
    );
}

#[test]
fn test_printf_precision_cap_errors() {
    let vars = test_vars();
    let result = render("{{ printf \"%.99999999f\" 1.0 }}", &vars);
    assert!(result.is_err());
    let msg = format!("{:?}", result.unwrap_err());
    assert!(
        msg.contains("printf precision") && msg.contains("exceeds maximum"),
        "got: {}",
        msg
    );
}

// --- slice: native-superset semantics (optional start, negative-from-end) ---

#[test]
fn test_slice_default_start() {
    let vars = test_vars();
    // start defaults to 0 (native Tera behavior).
    let result = render("{{ \"abcdef\" | slice(end=3) }}", &vars).unwrap();
    assert_eq!(result, "abc");
}

#[test]
fn test_slice_negative_start_string() {
    let vars = test_vars();
    // Negative start counts from the end: last 2 chars.
    let result = render("{{ \"abcdef\" | slice(start=-2) }}", &vars).unwrap();
    assert_eq!(result, "ef");
}

#[test]
fn test_slice_negative_start_array() {
    let mut vars = test_vars();
    vars.set_structured("Items", serde_json::json!(["a", "b", "c", "d"]));
    let result = render("{{ Items | slice(start=-2) | join(sep=\",\") }}", &vars).unwrap();
    assert_eq!(result, "c,d");
}

#[test]
fn test_slice_array_default_start() {
    let mut vars = test_vars();
    vars.set_structured("Items", serde_json::json!(["a", "b", "c", "d"]));
    let result = render("{{ Items | slice(end=2) | join(sep=\",\") }}", &vars).unwrap();
    assert_eq!(result, "a,b");
}

// --- print: Go Sprint spacing (space only between two non-strings) ---

#[test]
fn test_print_two_numbers_spaced() {
    let mut vars = test_vars();
    vars.set_structured("A", Value::Number(1i64.into()));
    vars.set_structured("B", Value::Number(2i64.into()));
    let result = render("{{ print A B }}", &vars).unwrap();
    assert_eq!(result, "1 2");
}

#[test]
fn test_print_two_strings_no_space() {
    let vars = test_vars();
    let result = render("{{ print \"a\" \"b\" }}", &vars).unwrap();
    assert_eq!(result, "ab");
}

#[test]
fn test_print_string_then_number_no_space() {
    let mut vars = test_vars();
    vars.set_structured("N", Value::Number(1i64.into()));
    let result = render("{{ print \"a\" N }}", &vars).unwrap();
    assert_eq!(result, "a1");
}

#[test]
fn test_printf_g_explicit_precision_exponent_trims_zeros() {
    let mut vars = test_vars();
    vars.set_structured(
        "V",
        Value::Number(serde_json::Number::from_f64(1200000.0).unwrap()),
    );
    // %.3g of 1.2e6: exponential branch, trailing mantissa zeros trimmed → 1.2e+06.
    let result = render("{{ printf \"%.3g\" V }}", &vars).unwrap();
    assert_eq!(result, "1.2e+06");
}

#[test]
fn test_render_preserves_emoji_in_literal_footer() {
    // End-to-end: a release-note footer with non-ASCII literal text must render
    // the emoji intact, not the Latin-1 double-encode mojibake (`ð...`).
    let vars = test_vars();
    let result = render("Released with [anodizer](https://x) 🦀", &vars).unwrap();
    assert_eq!(result, "Released with [anodizer](https://x) 🦀");
    assert!(!result.contains('\u{f0}'), "no mojibake `ð`: {result:?}");
}

// ---- Coverage: value_to_string Bool / Array / Object arms (lines 22-23, 25) ----

#[test]
fn test_value_to_string_bool_arm_via_in_function() {
    // `in` uses value_to_string on each item; a bool item exercises the Bool arm.
    let vars = test_vars();
    // The array contains a bool true; search for "true" (string match after stringification).
    let result = render(
        "{% set items = [true, false] %}{% if items | in(value=\"true\") %}yes{% else %}no{% endif %}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "yes");
}

#[test]
fn test_value_to_string_bool_false_arm_via_in_function() {
    // Bool false → "false" via value_to_string Bool arm.
    let vars = test_vars();
    let result = render(
        "{% set items = [true, false] %}{% if items | in(value=\"false\") %}yes{% else %}no{% endif %}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "yes");
}

#[test]
fn test_value_to_string_array_arm_via_printf_v() {
    // %v of an array falls back to JSON representation (the "other" arm in value_to_string).
    let mut vars = test_vars();
    vars.set_structured("Arr", serde_json::json!(["a", "b"]));
    let result = render("{{ printf \"%v\" Arr }}", &vars).unwrap();
    // JSON representation of an array: ["a","b"] (Tera/serde_json compact form).
    assert!(
        !result.is_empty(),
        "array should render via JSON fallback: {result}"
    );
    assert!(
        result.contains("a") && result.contains("b"),
        "got: {result}"
    );
}

// ---- Coverage: pad — len >= width (line 64) ----

#[test]
fn test_printf_string_already_wider_than_width() {
    // When the formatted value is already wider than the requested width,
    // pad returns it unchanged (line 64 — early return when len >= width).
    let vars = test_vars();
    let result = render("{{ printf \"%2s\" \"hello\" }}", &vars).unwrap();
    assert_eq!(result, "hello");
}

// ---- Coverage: pad zero-pad without sign/prefix (line 73) ----

#[test]
fn test_printf_zero_pad_string_verb() {
    // %0Ns with a string: zero-pad is applied (no sign prefix for strings).
    // This exercises the `else if spec.zero` branch (line 73) with None sign-prefix.
    let vars = test_vars();
    let result = render("{{ printf \"%05s\" \"hi\" }}", &vars).unwrap();
    // Go pads strings with spaces, not zeros — zero-pad only applies to numbers.
    // But our implementation routes through pad() with no sign prefix.
    // The format "%05s" with a space-pad/zero-pad branch: since no numeric_sign_prefix
    // the zero branch (line 73) applies → "000hi".
    assert_eq!(result, "000hi");
}

// ---- Coverage: numeric_sign space flag (line 105) ----

#[test]
fn test_printf_space_flag_positive_int() {
    // "% d" of a positive integer: space is prepended as sign (line 105 in numeric_sign).
    let vars = test_vars();
    let result = render("{{ printf \"% d\" 7 }}", &vars).unwrap();
    assert_eq!(result, " 7");
}

#[test]
fn test_printf_space_flag_negative_int() {
    // "% d" of a negative integer: minus sign wins over space (numeric_sign returns "-").
    let mut vars = test_vars();
    vars.set_structured("Neg", Value::Number((-3i64).into()));
    let result = render("{{ printf \"% d\" Neg }}", &vars).unwrap();
    assert_eq!(result, "-3");
}

// ---- Coverage: go_exponent — no e/E in string (line 136) ----
// go_exponent is called from format_g's exponential branch AND from %e/%E.
// The "no e/E" early return (line 136) is a defensive branch. It IS reachable
// from format_g's trim path when trim_fraction_zeros produces a value without
// an exponent -- but in practice the exponential branch always has an `e`.
// This branch cannot be reached through public API without instrumenting the
// private function; skip it.

// ---- Coverage: trim_fraction_zeros — no dot in string (line 158) ----

#[test]
fn test_printf_g_integer_value_no_dot() {
    // trim_fraction_zeros is called in format_g decimal branch.
    // format!("{}", 42.0_f64) → "42" (no dot in Rust), exercising line 158.
    let mut vars = test_vars();
    vars.set_structured(
        "V",
        Value::Number(serde_json::Number::from_f64(42.0).unwrap()),
    );
    let result = render("{{ printf \"%g\" V }}", &vars).unwrap();
    // exp=1 < eprec=6 → decimal branch; "42" has no dot, trim_fraction_zeros passes through.
    assert_eq!(result, "42");
}

// ---- Coverage: format_g decimal branch with explicit precision (lines 205-207) ----

#[test]
fn test_printf_g_explicit_precision_decimal_branch() {
    // %.4g of 3.14159: exp=0, eprec=4, 0 < 4 → decimal branch with explicit precision.
    // frac = (4 - (0+1)).max(0) = 3 fractional digits → "3.142", trimmed zeros → "3.142".
    let vars = test_vars();
    let result = render("{{ printf \"%.4g\" 3.14159 }}", &vars).unwrap();
    assert_eq!(result, "3.142");
}

#[test]
fn test_printf_g_explicit_precision_decimal_branch_rounds() {
    // %.2g of 3.14: exp=0, eprec=2 → decimal branch; frac=(2-1)=1 → "3.1" trimmed.
    let vars = test_vars();
    let result = render("{{ printf \"%.2g\" 3.14 }}", &vars).unwrap();
    assert_eq!(result, "3.1");
}

// ---- Coverage: sprintf literal %% (line 233) ----
// Already covered by test_printf_percent_literal — included in existing suite.
// Listing here as confirmation; no new test needed if the line is reported covered.

// ---- Coverage: format_verb %c — invalid code point (lines 257-258) ----

#[test]
fn test_printf_c_verb_invalid_code_point() {
    // A value > 0x10FFFF is not a valid Unicode scalar: %c must error.
    let mut vars = test_vars();
    vars.set_structured("Big", Value::Number((0x110000u64).into()));
    let result = render("{{ printf \"%c\" Big }}", &vars);
    assert!(result.is_err(), "%c with invalid code point must error");
    let msg = format!("{:?}", result.unwrap_err());
    assert!(
        msg.contains("valid code point") || msg.contains("%c"),
        "got: {msg}"
    );
}

// ---- Coverage: format_verb %d type error (line 275) ----

#[test]
fn test_printf_d_verb_string_type_error() {
    // %d expects an integer; passing a non-numeric string should error.
    let vars = test_vars();
    let result = render("{{ printf \"%d\" \"hello\" }}", &vars);
    assert!(result.is_err(), "%d with string must error");
    let msg = format!("{:?}", result.unwrap_err());
    assert!(
        msg.contains("expects an integer") || msg.contains("%d"),
        "got: {msg}"
    );
}

// ---- Coverage: format_verb %b/%o/%x/%X type error (line 279) ----

#[test]
fn test_printf_x_verb_string_type_error() {
    // %x expects an integer; a float string should error.
    let vars = test_vars();
    let result = render("{{ printf \"%x\" \"nope\" }}", &vars);
    assert!(result.is_err(), "%x with non-integer must error");
    let msg = format!("{:?}", result.unwrap_err());
    assert!(
        msg.contains("expects an integer") || msg.contains("%x"),
        "got: {msg}"
    );
}

#[test]
fn test_printf_o_verb_string_type_error() {
    let vars = test_vars();
    let result = render("{{ printf \"%o\" \"nope\" }}", &vars);
    assert!(result.is_err(), "%o with non-integer must error");
}

#[test]
fn test_printf_b_verb_string_type_error() {
    let vars = test_vars();
    let result = render("{{ printf \"%b\" \"nope\" }}", &vars);
    assert!(result.is_err(), "%b with non-integer must error");
}

// ---- Coverage: %b/%o prefix with # flag (lines 286, 292-296) ----

#[test]
fn test_printf_hash_binary_prefix() {
    // %#b of 5 → "0b101" (line 286 prefix "0b").
    let vars = test_vars();
    let result = render("{{ printf \"%#b\" 5 }}", &vars).unwrap();
    assert_eq!(result, "0b101");
}

#[test]
fn test_printf_hash_octal_prefix() {
    // %#o of 8 → "010" (line 292 prefix "0").
    let vars = test_vars();
    let result = render("{{ printf \"%#o\" 8 }}", &vars).unwrap();
    assert_eq!(result, "010");
}

#[test]
fn test_printf_hash_hex_upper_prefix() {
    // %#X of 255 → "0XFF" (line 295 prefix "0X").
    let vars = test_vars();
    let result = render("{{ printf \"%#X\" 255 }}", &vars).unwrap();
    assert_eq!(result, "0XFF");
}

// ---- Coverage: %f/%e/%E/%g/%G float type error (lines 305-306) ----

#[test]
fn test_printf_f_verb_string_type_error() {
    // %f expects a numeric argument; a string should error.
    let vars = test_vars();
    let result = render("{{ printf \"%f\" \"nope\" }}", &vars);
    assert!(result.is_err(), "%f with non-numeric must error");
    let msg = format!("{:?}", result.unwrap_err());
    assert!(
        msg.contains("expects a numeric") || msg.contains("%f"),
        "got: {msg}"
    );
}

#[test]
fn test_printf_e_verb_string_type_error() {
    let vars = test_vars();
    let result = render("{{ printf \"%e\" \"nope\" }}", &vars);
    assert!(result.is_err(), "%e with non-numeric must error");
}

// ---- Coverage: printf width overflow (line 369) ----
// Already covered by test_printf_width_cap_errors.

// ---- Coverage: sprintf empty precision "%.d" → precision 0 (line 413) ----

#[test]
fn test_printf_empty_precision_means_zero() {
    // "%.d" means precision=0; for %d, precision 0 of value 0 → empty string.
    let mut vars = test_vars();
    vars.set_structured("Z", Value::Number(0i64.into()));
    let result = render("{{ printf \"%.d\" Z }}", &vars).unwrap();
    assert_eq!(result, "");
}

#[test]
fn test_printf_empty_precision_nonzero_value() {
    // "%.d" of 42: precision 0 means min-digit-count 0, but 42 already has 2 digits.
    let vars = test_vars();
    let result = render("{{ printf \"%.d\" 42 }}", &vars).unwrap();
    assert_eq!(result, "42");
}

// ---- Coverage: BASE_TERA placeholder envOrDefault (lines 628-636) ----
// The placeholder in BASE_TERA reads directly from std::env::var when called
// without the render() override. Exercised by calling BASE_TERA directly.

#[test]
fn test_base_tera_env_or_default_placeholder_reads_process_env() {
    use super::base_tera::BASE_TERA;
    // Set a unique env var, then call the BASE_TERA placeholder directly.
    let key = "ANODIZER_TEST_PLACEHOLDER_ENVDEFAULT";
    // Ensure the var is absent — should return the default.
    unsafe { std::env::remove_var(key) };
    let mut tera = BASE_TERA.clone();
    tera.add_raw_template(
        "t",
        "{{ envOrDefault(name=\"ANODIZER_TEST_PLACEHOLDER_ENVDEFAULT\", default=\"sentinel\") }}",
    )
    .unwrap();
    let ctx = tera::Context::new();
    let result = tera.render("t", &ctx).unwrap();
    assert_eq!(result, "sentinel");
}

#[test]
fn test_base_tera_env_or_default_placeholder_reads_set_var() {
    use super::base_tera::BASE_TERA;
    let key = "ANODIZER_TEST_PLACEHOLDER_ENVSET";
    unsafe { std::env::set_var(key, "fromenv") };
    let mut tera = BASE_TERA.clone();
    tera.add_raw_template(
        "t",
        "{{ envOrDefault(name=\"ANODIZER_TEST_PLACEHOLDER_ENVSET\", default=\"missed\") }}",
    )
    .unwrap();
    let ctx = tera::Context::new();
    let result = tera.render("t", &ctx).unwrap();
    unsafe { std::env::remove_var(key) };
    assert_eq!(result, "fromenv");
}

// ---- Coverage: BASE_TERA placeholder isEnvSet (lines 640-647) ----

#[test]
fn test_base_tera_is_env_set_placeholder_unset() {
    use super::base_tera::BASE_TERA;
    let key = "ANODIZER_TEST_PLACEHOLDER_ISSET_UNSET";
    unsafe { std::env::remove_var(key) };
    let mut tera = BASE_TERA.clone();
    tera.add_raw_template("t", "{% if isEnvSet(name=\"ANODIZER_TEST_PLACEHOLDER_ISSET_UNSET\") %}set{% else %}unset{% endif %}")
        .unwrap();
    let ctx = tera::Context::new();
    let result = tera.render("t", &ctx).unwrap();
    assert_eq!(result, "unset");
}

#[test]
fn test_base_tera_is_env_set_placeholder_set() {
    use super::base_tera::BASE_TERA;
    let key = "ANODIZER_TEST_PLACEHOLDER_ISSET_SET";
    unsafe { std::env::set_var(key, "yes") };
    let mut tera = BASE_TERA.clone();
    tera.add_raw_template("t", "{% if isEnvSet(name=\"ANODIZER_TEST_PLACEHOLDER_ISSET_SET\") %}set{% else %}unset{% endif %}")
        .unwrap();
    let ctx = tera::Context::new();
    let result = tera.render("t", &ctx).unwrap();
    unsafe { std::env::remove_var(key) };
    assert_eq!(result, "set");
}

// ---- Coverage: hash fn file-read error (lines 700-701) ----

#[test]
fn test_hash_sha256_missing_file_errors() {
    // sha256 (and all hash fns) must error (not silently return "") on a missing file.
    let vars = test_vars();
    let result = render(
        "{{ sha256(s=\"/nonexistent/path/anodizer_test_missing_abc\") }}",
        &vars,
    );
    assert!(result.is_err(), "hash of missing file must error");
    let msg = format!("{:?}", result.unwrap_err());
    assert!(
        msg.contains("failed to read file") || msg.contains("sha256"),
        "got: {msg}"
    );
}

// ---- Coverage: sha224 (lines 714-717) ----

#[test]
fn test_hash_sha224() {
    let vars = test_vars();
    let (_dir, path) = hash_test_file();
    let tmpl = format!("{{{{ sha224(s=\"{path}\") }}}}");
    let result = render(&tmpl, &vars).unwrap();
    // Known SHA-224 of "hello": ea09ae9cc6768c50fcee903ed054556e5bfc8347907f12598aa24193
    assert_eq!(
        result,
        "ea09ae9cc6768c50fcee903ed054556e5bfc8347907f12598aa24193"
    );
}

// ---- Coverage: sha384 (lines 724-727) ----

#[test]
fn test_hash_sha384() {
    let vars = test_vars();
    let (_dir, path) = hash_test_file();
    let tmpl = format!("{{{{ sha384(s=\"{path}\") }}}}");
    let result = render(&tmpl, &vars).unwrap();
    // Known SHA-384 of "hello"
    assert_eq!(
        result,
        "59e1748777448c69de6b800d7a33bbfb9ff1b463e44354c3553bcdb9c666fa90125a3c79f90397bdf5f6a13de828684f"
    );
}

// ---- Coverage: sha3_224 (lines 734-737) ----

#[test]
fn test_hash_sha3_224() {
    let vars = test_vars();
    let (_dir, path) = hash_test_file();
    let tmpl = format!("{{{{ sha3_224(s=\"{path}\") }}}}");
    let result = render(&tmpl, &vars).unwrap();
    // Known SHA3-224 of "hello"
    assert_eq!(
        result,
        "b87f88c72702fff1748e58b87e9141a42c0dbedc29a78cb0d4a5cd81"
    );
}

// ---- Coverage: sha3_256 (lines 739-742) ----

#[test]
fn test_hash_sha3_256() {
    let vars = test_vars();
    let (_dir, path) = hash_test_file();
    let tmpl = format!("{{{{ sha3_256(s=\"{path}\") }}}}");
    let result = render(&tmpl, &vars).unwrap();
    // Known SHA3-256 of "hello"
    assert_eq!(
        result,
        "3338be694f50c5f338814986cdf0686453a888b84f424d792af4b9202398f392"
    );
}

// ---- Coverage: sha3_384 (lines 744-747) ----

#[test]
fn test_hash_sha3_384() {
    let vars = test_vars();
    let (_dir, path) = hash_test_file();
    let tmpl = format!("{{{{ sha3_384(s=\"{path}\") }}}}");
    let result = render(&tmpl, &vars).unwrap();
    // Known SHA3-384 of "hello"
    assert_eq!(
        result,
        "720aea11019ef06440fbf05d87aa24680a2153df3907b23631e7177ce620fa1330ff07c0fddee54699a4c3ee0ee9d887"
    );
}

// ---- Coverage: sha3_512 (lines 749-752) ----

#[test]
fn test_hash_sha3_512() {
    let vars = test_vars();
    let (_dir, path) = hash_test_file();
    let tmpl = format!("{{{{ sha3_512(s=\"{path}\") }}}}");
    let result = render(&tmpl, &vars).unwrap();
    // Known SHA3-512 of "hello"
    assert_eq!(
        result,
        "75d527c368f2efe848ecf6b073a36767800805e9eef2b1857d5f984f036eb6df891d75f72d9b154518c1cd58835286d1da9a38deba3de98b5a53e5ed78a84976"
    );
}

// ---- Coverage: blake2b (lines 754-757) ----

#[test]
fn test_hash_blake2b() {
    let vars = test_vars();
    let (_dir, path) = hash_test_file();
    let tmpl = format!("{{{{ blake2b(s=\"{path}\") }}}}");
    let result = render(&tmpl, &vars).unwrap();
    // blake2b-512 of "hello" — known vector
    assert_eq!(
        result,
        "e4cfa39a3d37be31c59609e807970799caa68a19bfaa15135f165085e01d41a65ba1e1b146aeb6bd0092b49eac214c103ccfa3a365954bbbe52f74a2b3620c94"
    );
}

// ---- Coverage: blake2s (lines 759-762) ----

#[test]
fn test_hash_blake2s() {
    let vars = test_vars();
    let (_dir, path) = hash_test_file();
    let tmpl = format!("{{{{ blake2s(s=\"{path}\") }}}}");
    let result = render(&tmpl, &vars).unwrap();
    // blake2s-256 of "hello" — known vector
    assert_eq!(
        result,
        "19213bacc58dee6dbde3ceb9a47cbb330b3d86f8cca8997eb00be456f140ca25"
    );
}

// ---- Coverage: BASE_TERA time placeholder (lines 819-827) ----

#[test]
fn test_base_tera_time_placeholder_produces_date() {
    use super::base_tera::BASE_TERA;
    let mut tera = BASE_TERA.clone();
    tera.add_raw_template("t", "{{ time(format=\"%Y-%m-%d\") }}")
        .unwrap();
    let ctx = tera::Context::new();
    let result = tera.render("t", &ctx).unwrap();
    assert_eq!(result.len(), 10, "expected YYYY-MM-DD, got: {result}");
    assert_eq!(result.matches('-').count(), 2);
}

// ---- Coverage: abs filter — relative path (lines 856-866) ----

#[test]
fn test_abs_filter_relative_path() {
    let mut vars = test_vars();
    vars.set("RelPath", "some/relative/path");
    let result = render("{{ RelPath | abs }}", &vars).unwrap();
    // A relative path should be prefixed with the cwd.
    assert!(
        std::path::Path::new(&result).is_absolute(),
        "abs filter on relative path must return absolute path: {result}"
    );
    assert!(result.ends_with("some/relative/path"), "got: {result}");
}

#[test]
fn test_abs_filter_absolute_path_passthrough() {
    let mut vars = test_vars();
    // `abs` returns an already-absolute path verbatim; "absolute" is
    // host-defined — a leading-slash path is not absolute on Windows
    // (it needs a drive prefix), so pick a per-host absolute path.
    let already_abs = if cfg!(windows) {
        "C:/already/absolute"
    } else {
        "/already/absolute"
    };
    vars.set("AbsPath", already_abs);
    let result = render("{{ AbsPath | abs }}", &vars).unwrap();
    assert_eq!(result, already_abs);
}

// ---- Coverage: list function (lines 931-937) ----

#[test]
fn test_list_function_creates_array() {
    let vars = test_vars();
    let result = render(
        "{{ list(items=[\"x\", \"y\", \"z\"]) | join(sep=\",\") }}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "x,y,z");
}

#[test]
fn test_list_function_missing_items_error() {
    let vars = test_vars();
    let result = render("{{ list() }}", &vars);
    assert!(result.is_err(), "list without items should error");
}

// ---- Coverage: map function — odd args error (lines 950-952) ----

#[test]
fn test_map_function_odd_args_error() {
    // map with an odd number of key-value pairs must error.
    let vars = test_vars();
    let result = render("{{ $m := map \"a\" \"1\" \"b\" }}{{ $m }}", &vars);
    assert!(result.is_err(), "map with odd arg count must error");
    let msg = format!("{:?}", result.unwrap_err());
    assert!(
        msg.contains("even") || msg.contains("key-value"),
        "got: {msg}"
    );
}

// ---- Coverage: reReplaceAll filter — missing pattern/replacement (lines 1027-1034) ----

#[test]
fn test_re_replace_all_filter_missing_pattern_error() {
    // Filter form without pattern arg must error.
    let vars = test_vars();
    let result = render("{{ Tag | reReplaceAll(replacement=\"x\") }}", &vars);
    assert!(
        result.is_err(),
        "reReplaceAll filter without pattern should error"
    );
    let msg = format!("{:?}", result.unwrap_err());
    assert!(
        msg.contains("pattern") || msg.contains("reReplaceAll"),
        "got: {msg}"
    );
}

#[test]
fn test_re_replace_all_filter_missing_replacement_error() {
    // Filter form without replacement arg must error.
    let vars = test_vars();
    let result = render("{{ Tag | reReplaceAll(pattern=\"v\") }}", &vars);
    assert!(
        result.is_err(),
        "reReplaceAll filter without replacement should error"
    );
    let msg = format!("{:?}", result.unwrap_err());
    assert!(
        msg.contains("replacement") || msg.contains("reReplaceAll"),
        "got: {msg}"
    );
}

// ---- Coverage: englishJoin filter form (lines 1094-1124) ----

#[test]
fn test_english_join_filter_zero_items() {
    // Pipe form: empty array through englishJoin filter.
    let mut vars = test_vars();
    vars.set_structured("Names", Value::Array(vec![]));
    let result = render("{{ Names | englishJoin }}", &vars).unwrap();
    assert_eq!(result, "");
}

#[test]
fn test_english_join_filter_one_item() {
    let mut vars = test_vars();
    vars.set_structured("Names", Value::Array(vec![Value::String("alice".into())]));
    let result = render("{{ Names | englishJoin }}", &vars).unwrap();
    assert_eq!(result, "alice");
}

#[test]
fn test_english_join_filter_two_items() {
    let mut vars = test_vars();
    vars.set_structured(
        "Names",
        Value::Array(vec![
            Value::String("alice".into()),
            Value::String("bob".into()),
        ]),
    );
    let result = render("{{ Names | englishJoin }}", &vars).unwrap();
    assert_eq!(result, "alice and bob");
}

#[test]
fn test_english_join_filter_three_items_no_oxford() {
    let mut vars = test_vars();
    vars.set_structured(
        "Names",
        Value::Array(vec![
            Value::String("a".into()),
            Value::String("b".into()),
            Value::String("c".into()),
        ]),
    );
    let result = render("{{ Names | englishJoin(oxford=false) }}", &vars).unwrap();
    assert_eq!(result, "a, b and c");
}

#[test]
fn test_english_join_filter_non_array_errors() {
    let vars = test_vars();
    let result = render("{{ ProjectName | englishJoin }}", &vars);
    assert!(
        result.is_err(),
        "englishJoin filter on non-array must error"
    );
}

// ---- Coverage: reverseFilter filter — missing arg (lines 1137-1139) ----

#[test]
fn test_reverse_filter_pipe_missing_arg_error() {
    let vars = test_vars();
    // reverseFilter pipe form without regexp should error.
    let result = render("{{ ProjectName | reverseFilter }}", &vars);
    assert!(result.is_err(), "reverseFilter without regexp must error");
}

// ---- Coverage: filter fn — string items input (lines 1163-1168) ----

#[test]
fn test_filter_function_string_input_keeps_matching_lines() {
    // filter(items=multiline_string, regexp="2") → "line2"
    // Pass the multiline string via a structured var so Tera sees the real newlines.
    let mut vars = test_vars();
    vars.set_structured("Lines", Value::String("line1\nline2\nline3".to_string()));
    let result = render("{{ filter(items=Lines, regexp=\"2\") }}", &vars).unwrap();
    assert_eq!(result, "line2");
}

#[test]
fn test_filter_function_string_input_no_match() {
    let mut vars = test_vars();
    vars.set_structured("Lines", Value::String("aaa\nbbb\nccc".to_string()));
    let result = render("{{ filter(items=Lines, regexp=\"xyz\") }}", &vars).unwrap();
    assert_eq!(result, "");
}

// ---- Coverage: filter fn — invalid type error (lines 1178-1180) ----

#[test]
fn test_filter_function_number_items_error() {
    // Passing a number (not string or array) as items must error.
    let mut vars = test_vars();
    vars.set_structured("N", Value::Number(42i64.into()));
    let result = render("{{ filter(items=N, regexp=\"x\") }}", &vars);
    assert!(
        result.is_err(),
        "filter with non-string/non-array items must error"
    );
    let msg = format!("{:?}", result.unwrap_err());
    assert!(
        msg.contains("string or array") || msg.contains("filter"),
        "got: {msg}"
    );
}

// ---- Coverage: reverseFilter fn (lines 1205-1244) ----

#[test]
fn test_reverse_filter_function_string_input() {
    // String input: reverseFilter keeps lines NOT matching the regex.
    // Pass the multiline string via a structured var so Tera sees real newlines.
    let mut vars = test_vars();
    vars.set_structured("Lines", Value::String("line1\nline2\nline3".to_string()));
    let result = render("{{ reverseFilter(items=Lines, regexp=\"2\") }}", &vars).unwrap();
    // Should keep line1 and line3, not line2.
    assert!(result.contains("line1"), "got: {result}");
    assert!(!result.contains("line2"), "got: {result}");
    assert!(result.contains("line3"), "got: {result}");
}

#[test]
fn test_reverse_filter_function_array_input() {
    // Array input: reverseFilter excludes matching elements.
    let vars = test_vars();
    let result = render(
        "{{ reverseFilter(items=[\"apple\", \"banana\", \"avocado\"], regexp=\"^a\") }}",
        &vars,
    )
    .unwrap();
    assert!(result.contains("banana"), "got: {result}");
    assert!(!result.contains("apple"), "got: {result}");
    assert!(!result.contains("avocado"), "got: {result}");
}

#[test]
fn test_reverse_filter_function_invalid_type_error() {
    let mut vars = test_vars();
    vars.set_structured("N", Value::Number(42i64.into()));
    let result = render("{{ reverseFilter(items=N, regexp=\"x\") }}", &vars);
    assert!(
        result.is_err(),
        "reverseFilter with non-string/non-array must error"
    );
    let msg = format!("{:?}", result.unwrap_err());
    assert!(
        msg.contains("string or array") || msg.contains("reverseFilter"),
        "got: {msg}"
    );
}

#[test]
fn test_reverse_filter_function_missing_items_error() {
    let vars = test_vars();
    let result = render("{{ reverseFilter(regexp=\"x\") }}", &vars);
    assert!(result.is_err(), "reverseFilter without items must error");
}

#[test]
fn test_reverse_filter_function_missing_regexp_error() {
    let vars = test_vars();
    let result = render("{{ reverseFilter(items=[\"a\", \"b\"]) }}", &vars);
    assert!(result.is_err(), "reverseFilter without regexp must error");
}

// ---- Coverage: dual-registered function forms (lines 1329-1565) ----

#[test]
fn test_trim_function_form() {
    let vars = test_vars();
    let result = render("{{ trim(s=\"  hello  \") }}", &vars).unwrap();
    assert_eq!(result, "hello");
}

#[test]
fn test_trim_function_form_missing_s_error() {
    let vars = test_vars();
    let result = render("{{ trim() }}", &vars);
    assert!(result.is_err(), "trim() without s must error");
}

#[test]
fn test_title_function_form() {
    let vars = test_vars();
    let result = render("{{ title(s=\"hello world\") }}", &vars).unwrap();
    assert_eq!(result, "Hello World");
}

#[test]
fn test_title_function_form_missing_s_error() {
    let vars = test_vars();
    let result = render("{{ title() }}", &vars);
    assert!(result.is_err(), "title() without s must error");
}

#[test]
fn test_tolower_function_form() {
    let vars = test_vars();
    let result = render("{{ tolower(s=\"HELLO\") }}", &vars).unwrap();
    assert_eq!(result, "hello");
}

#[test]
fn test_tolower_function_form_missing_s_error() {
    let vars = test_vars();
    let result = render("{{ tolower() }}", &vars);
    assert!(result.is_err());
}

#[test]
fn test_toupper_function_form() {
    let vars = test_vars();
    let result = render("{{ toupper(s=\"hello\") }}", &vars).unwrap();
    assert_eq!(result, "HELLO");
}

#[test]
fn test_toupper_function_form_missing_s_error() {
    let vars = test_vars();
    let result = render("{{ toupper() }}", &vars);
    assert!(result.is_err());
}

#[test]
fn test_trimprefix_function_form() {
    let vars = test_vars();
    let result = render("{{ trimprefix(s=\"v1.2.3\", prefix=\"v\") }}", &vars).unwrap();
    assert_eq!(result, "1.2.3");
}

#[test]
fn test_trimprefix_function_form_no_match() {
    let vars = test_vars();
    let result = render("{{ trimprefix(s=\"1.2.3\", prefix=\"v\") }}", &vars).unwrap();
    assert_eq!(result, "1.2.3");
}

#[test]
fn test_trimprefix_function_form_missing_args_error() {
    let vars = test_vars();
    let result = render("{{ trimprefix(s=\"v1.2.3\") }}", &vars);
    assert!(result.is_err(), "trimprefix fn without prefix should error");
}

#[test]
fn test_trimsuffix_function_form() {
    let vars = test_vars();
    let result = render(
        "{{ trimsuffix(s=\"hello.tar.gz\", suffix=\".tar.gz\") }}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "hello");
}

#[test]
fn test_trimsuffix_function_form_no_match() {
    let vars = test_vars();
    let result = render(
        "{{ trimsuffix(s=\"hello.zip\", suffix=\".tar.gz\") }}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "hello.zip");
}

#[test]
fn test_trimsuffix_function_form_missing_args_error() {
    let vars = test_vars();
    let result = render("{{ trimsuffix(s=\"hello\") }}", &vars);
    assert!(result.is_err(), "trimsuffix fn without suffix should error");
}

#[test]
fn test_dir_function_form() {
    let vars = test_vars();
    let result = render("{{ dir(s=\"/foo/bar/baz.txt\") }}", &vars).unwrap();
    assert_eq!(result, "/foo/bar");
}

#[test]
fn test_dir_function_form_missing_s_error() {
    let vars = test_vars();
    let result = render("{{ dir() }}", &vars);
    assert!(result.is_err());
}

#[test]
fn test_base_function_form() {
    let vars = test_vars();
    let result = render("{{ base(s=\"/foo/bar/baz.txt\") }}", &vars).unwrap();
    assert_eq!(result, "baz.txt");
}

#[test]
fn test_base_function_form_missing_s_error() {
    let vars = test_vars();
    let result = render("{{ base() }}", &vars);
    assert!(result.is_err());
}

#[test]
fn test_abs_function_form_absolute() {
    let vars = test_vars();
    // See `test_abs_filter_absolute_path_passthrough`: absoluteness is
    // host-defined, so use a per-host absolute path the function passes
    // through unchanged.
    let already_abs = if cfg!(windows) {
        "C:/already/absolute"
    } else {
        "/already/absolute"
    };
    let result = render(&format!("{{{{ abs(s=\"{already_abs}\") }}}}"), &vars).unwrap();
    assert_eq!(result, already_abs);
}

#[test]
fn test_abs_function_form_relative() {
    let vars = test_vars();
    let result = render("{{ abs(s=\"relative/path\") }}", &vars).unwrap();
    assert!(
        std::path::Path::new(&result).is_absolute(),
        "abs(s=relative) must be absolute: {result}"
    );
    assert!(result.ends_with("relative/path"), "got: {result}");
}

#[test]
fn test_abs_function_form_missing_s_error() {
    let vars = test_vars();
    let result = render("{{ abs() }}", &vars);
    assert!(result.is_err());
}

#[test]
fn test_url_path_escape_function_form() {
    let vars = test_vars();
    let result = render("{{ urlPathEscape(s=\"hello world\") }}", &vars).unwrap();
    assert_eq!(result, "hello%20world");
}

#[test]
fn test_url_path_escape_function_form_missing_s_error() {
    let vars = test_vars();
    let result = render("{{ urlPathEscape() }}", &vars);
    assert!(result.is_err());
}

#[test]
fn test_mdv2escape_function_form() {
    let vars = test_vars();
    let result = render("{{ mdv2escape(s=\"hello_world\") }}", &vars).unwrap();
    assert_eq!(result, "hello\\_world");
}

#[test]
fn test_mdv2escape_function_form_missing_s_error() {
    let vars = test_vars();
    let result = render("{{ mdv2escape() }}", &vars);
    assert!(result.is_err());
}

#[test]
fn test_incpatch_filter_form_increments() {
    let vars = test_vars();
    let result = render("{{ \"2.4.6\" | incpatch }}", &vars).unwrap();
    assert_eq!(result, "2.4.7");
}

#[test]
fn test_incminor_filter_form_increments() {
    let vars = test_vars();
    let result = render("{{ \"2.4.6\" | incminor }}", &vars).unwrap();
    assert_eq!(result, "2.5.0");
}

#[test]
fn test_incmajor_filter_form_increments() {
    let vars = test_vars();
    let result = render("{{ \"2.4.6\" | incmajor }}", &vars).unwrap();
    assert_eq!(result, "3.0.0");
}

// ---- Coverage: index — array non-number index error (line 1602) ----

#[test]
fn test_index_array_string_key_errors() {
    let mut vars = test_vars();
    vars.set_structured("Arr", serde_json::json!(["a", "b", "c"]));
    let result = render("{{ index(collection=Arr, key=\"notanumber\") }}", &vars);
    assert!(result.is_err(), "index on array with string key must error");
    let msg = format!("{:?}", result.unwrap_err());
    assert!(
        msg.contains("array index must be a number") || msg.contains("index"),
        "got: {msg}"
    );
}

// ---- Coverage: index — non-collection type (line 1597/1602) ----

#[test]
fn test_index_non_collection_returns_empty() {
    // Passing a scalar (string) as collection: graceful empty string.
    let vars = test_vars();
    let result = render("{{ index(collection=\"scalar\", key=\"k\") }}", &vars).unwrap();
    assert_eq!(result, "");
}

// ---- Coverage: slice filter — non-string/non-array type error (lines 1655-1658) ----

#[test]
fn test_slice_filter_number_errors() {
    // Slicing a number (not a string or array) must error.
    let mut vars = test_vars();
    vars.set_structured("N", Value::Number(42i64.into()));
    let result = render("{{ N | slice(start=0, end=1) }}", &vars);
    assert!(result.is_err(), "slice of a number must error");
    let msg = format!("{:?}", result.unwrap_err());
    assert!(
        msg.contains("string or array") || msg.contains("slice"),
        "got: {msg}"
    );
}

// ---- Coverage: printf %e / %E exponential verb formatting ----

#[test]
fn test_printf_e_lowercase_default_precision() {
    // Go %e default precision is 6; exponent is signed with min two digits.
    let mut vars = test_vars();
    vars.set_structured(
        "F",
        Value::Number(serde_json::Number::from_f64(1234.5).unwrap()),
    );
    let result = render("{{ printf \"%e\" F }}", &vars).unwrap();
    assert_eq!(result, "1.234500e+03");
}

#[test]
fn test_printf_e_uppercase_uses_capital_exponent_letter() {
    let mut vars = test_vars();
    vars.set_structured(
        "F",
        Value::Number(serde_json::Number::from_f64(1234.5).unwrap()),
    );
    let result = render("{{ printf \"%E\" F }}", &vars).unwrap();
    assert_eq!(result, "1.234500E+03");
}

#[test]
fn test_printf_e_explicit_precision_two() {
    let mut vars = test_vars();
    vars.set_structured(
        "F",
        Value::Number(serde_json::Number::from_f64(0.0001234).unwrap()),
    );
    let result = render("{{ printf \"%.2e\" F }}", &vars).unwrap();
    assert_eq!(result, "1.23e-04");
}

#[test]
fn test_printf_e_negative_value_keeps_sign_before_mantissa() {
    let mut vars = test_vars();
    vars.set_structured(
        "F",
        Value::Number(serde_json::Number::from_f64(-1234.5).unwrap()),
    );
    let result = render("{{ printf \"%e\" F }}", &vars).unwrap();
    assert_eq!(result, "-1.234500e+03");
}

// ---- Coverage: split function form (split(s=, sep=)) ----

#[test]
fn test_split_function_form_returns_array() {
    // The function form returns an array; rejoin with a different separator
    // to pin the element boundaries.
    let vars = test_vars();
    let result = render(
        "{{ split(s=\"a,b,c\", sep=\",\") | join(sep=\"|\") }}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "a|b|c");
}

#[test]
fn test_split_function_form_single_field_no_separator_match() {
    // No separator present → a single-element array containing the whole string.
    let vars = test_vars();
    let result = render(
        "{{ split(s=\"nodelimiter\", sep=\",\") | join(sep=\"|\") }}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "nodelimiter");
}

#[test]
fn test_split_function_form_missing_sep_errors() {
    let vars = test_vars();
    let result = render("{{ split(s=\"a,b\") }}", &vars);
    assert!(result.is_err(), "split without sep must error");
    let msg = format!("{:?}", result.unwrap_err());
    assert!(msg.contains("sep") || msg.contains("split"), "got: {msg}");
}

// ---- Coverage: map(pairs=[...]) named function form + odd-pairs error ----

#[test]
fn test_map_pairs_named_form_builds_object() {
    let vars = test_vars();
    let result = render(
        "{% set m = map(pairs=[\"k1\", \"v1\", \"k2\", \"v2\"]) %}{{ index(collection=m, key=\"k2\") }}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "v2");
}

#[test]
fn test_map_pairs_odd_count_errors() {
    let vars = test_vars();
    let result = render("{{ map(pairs=[\"k1\", \"v1\", \"k2\"]) }}", &vars);
    assert!(result.is_err(), "odd pair count must error");
    let msg = format!("{:?}", result.unwrap_err());
    assert!(
        msg.contains("even number") || msg.contains("key-value"),
        "got: {msg}"
    );
}

#[test]
fn test_map_pairs_missing_pairs_errors() {
    let vars = test_vars();
    let result = render("{{ map() }}", &vars);
    assert!(result.is_err(), "map without pairs must error");
    let msg = format!("{:?}", result.unwrap_err());
    assert!(msg.contains("pairs") || msg.contains("map"), "got: {msg}");
}

// ---- Coverage: contains_any (function + filter forms, the `in` alias) ----

#[test]
fn test_contains_any_function_form_found() {
    let vars = test_vars();
    let result = render(
        "{% if contains_any(items=[\"a\", \"b\", \"c\"], value=\"b\") %}yes{% else %}no{% endif %}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "yes");
}

#[test]
fn test_contains_any_function_form_not_found() {
    let vars = test_vars();
    let result = render(
        "{% if contains_any(items=[\"a\", \"b\"], value=\"z\") %}yes{% else %}no{% endif %}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "no");
}

#[test]
fn test_contains_any_filter_form_found() {
    let vars = test_vars();
    let result = render(
        "{% set items = [\"x\", \"y\"] %}{% if items | contains_any(value=\"y\") %}yes{% else %}no{% endif %}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "yes");
}

#[test]
fn test_contains_any_filter_form_numeric_value_stringified() {
    // value_to_string stringifies a numeric needle to match a numeric element.
    let vars = test_vars();
    let result = render(
        "{% set items = [1, 2, 3] %}{% if items | contains_any(value=2) %}yes{% else %}no{% endif %}",
        &vars,
    )
    .unwrap();
    assert_eq!(result, "yes");
}

// ---------------------------------------------------------------------------
// Typed bool/number template vars (set_bool / structured injection)
// ---------------------------------------------------------------------------

#[test]
fn test_bool_var_interpolates_as_true_false_strings() {
    // `{{ IsSnapshot }}` users must keep getting "true"/"false" text after
    // the switch from string injection to Value::Bool.
    let mut vars = TemplateVars::new();
    vars.set_bool("IsSnapshot", true);
    assert_eq!(render("{{ IsSnapshot }}", &vars).unwrap(), "true");
    vars.set_bool("IsSnapshot", false);
    assert_eq!(render("{{ IsSnapshot }}", &vars).unwrap(), "false");
    // Go-style spelling renders identically.
    assert_eq!(render("{{ .IsSnapshot }}", &vars).unwrap(), "false");
}

#[test]
fn test_bool_var_not_and_bare_if_evaluate_correctly() {
    let mut vars = TemplateVars::new();

    // Snapshot mode: `not IsSnapshot` false, bare truthiness true.
    vars.set_bool("IsSnapshot", true);
    assert_eq!(render("{{ not IsSnapshot }}", &vars).unwrap(), "false");
    assert_eq!(
        render("{% if IsSnapshot %}SNAP{% else %}REL{% endif %}", &vars).unwrap(),
        "SNAP"
    );

    // Release mode: inverse.
    vars.set_bool("IsSnapshot", false);
    assert_eq!(render("{{ not IsSnapshot }}", &vars).unwrap(), "true");
    assert_eq!(
        render("{% if IsSnapshot %}SNAP{% else %}REL{% endif %}", &vars).unwrap(),
        "REL"
    );
    // Go-style `{{ if not .IsSnapshot }}` via the preprocessor.
    assert_eq!(
        render("{{ if not .IsSnapshot }}REL{{ else }}SNAP{{ end }}", &vars).unwrap(),
        "REL"
    );
}

#[test]
fn test_bool_vars_combine_with_and_or() {
    let mut vars = TemplateVars::new();
    vars.set_bool("IsSnapshot", false);
    vars.set_bool("IsHarness", true);
    assert_eq!(
        render("{{ not IsSnapshot or IsHarness }}", &vars).unwrap(),
        "true"
    );
    vars.set_bool("IsHarness", false);
    assert_eq!(
        render("{{ not IsSnapshot or IsHarness }}", &vars).unwrap(),
        "true"
    );
    vars.set_bool("IsSnapshot", true);
    assert_eq!(
        render("{{ not IsSnapshot or IsHarness }}", &vars).unwrap(),
        "false"
    );
}

#[test]
fn test_nightly_build_number_interpolates_and_compares() {
    let mut vars = TemplateVars::new();
    vars.set_structured("NightlyBuild", Value::from(42u64));
    assert_eq!(render("{{ NightlyBuild }}", &vars).unwrap(), "42");
    assert_eq!(
        render(
            "{% if NightlyBuild == 42 %}yes{% else %}no{% endif %}",
            &vars
        )
        .unwrap(),
        "yes"
    );
    assert_eq!(
        render("{% if NightlyBuild > 0 %}yes{% else %}no{% endif %}", &vars).unwrap(),
        "yes"
    );
}

#[test]
fn test_set_and_set_structured_are_mutually_exclusive() {
    // A key must never resolve from both maps: whichever setter ran last
    // owns the key, so a test overriding a typed flag with `set` (string)
    // takes effect instead of being shadowed by the stale structured entry.
    let mut vars = TemplateVars::new();
    vars.set_bool("IsSnapshot", false);
    vars.set("IsSnapshot", "true");
    assert!(vars.get_structured("IsSnapshot").is_none());
    // The "true"/"false" string-coercion heuristic keeps bool semantics.
    assert_eq!(render("{{ not IsSnapshot }}", &vars).unwrap(), "false");

    vars.set_bool("IsSnapshot", false);
    assert!(vars.get("IsSnapshot").is_none());
    assert_eq!(vars.get_structured("IsSnapshot"), Some(&Value::Bool(false)));
}

#[test]
fn test_find_stale_typed_compare_detects_string_compares() {
    for tpl in [
        r#"{% if IsSnapshot == "false" %}true{% endif %}"#,
        r#"{% if IsSnapshot == "false" or IsHarness == "true" %}true{% endif %}"#,
        r#"{{ IsHarness != 'true' }}"#,
        r#"{{ "true" == .IsNightly }}"#,
        r#"{{ if eq .IsSnapshot "false" }}true{{ end }}"#,
        r#"{{ if ne .IsDraft "true" }}x{{ end }}"#,
        r#"{% if NightlyBuild == "0" %}first{% endif %}"#,
    ] {
        assert!(
            find_stale_typed_compare(tpl).is_some(),
            "must flag stale typed compare: {tpl}"
        );
    }
}

#[test]
fn test_find_stale_typed_compare_allows_natural_and_unrelated_forms() {
    for tpl in [
        "{{ not IsSnapshot }}",
        "{{ not IsSnapshot or IsHarness }}",
        "{% if IsSnapshot %}snap{% endif %}",
        "{{ if not .IsSnapshot }}rel{{ end }}",
        r#"{% if SomeUserVar == "false" %}x{% endif %}"#,
        r#"{% if GitTreeState == "dirty" %}x{% endif %}"#,
        "{% if NightlyBuild == 0 %}first{% endif %}",
        "{% if IsSnapshot == false %}rel{% endif %}",
    ] {
        assert!(
            find_stale_typed_compare(tpl).is_none(),
            "must not flag: {tpl}"
        );
    }
}

#[test]
fn test_find_stale_typed_compare_ignores_namespaced_user_vars() {
    // `Var.IsSnapshot` is a user custom var that merely shares the name;
    // only the top-level typed flags are lint targets.
    for tpl in [
        r#"{% if Var.IsSnapshot == "false" %}x{% endif %}"#,
        r#"{{ eq .Var.IsSnapshot "false" }}"#,
    ] {
        assert!(
            find_stale_typed_compare(tpl).is_none(),
            "must not flag namespaced user var: {tpl}"
        );
    }
    // Snippet returned is the compare itself, not the leading boundary.
    assert_eq!(
        find_stale_typed_compare(r#"{% if IsSnapshot == "false" %}x{% endif %}"#),
        Some(r#"IsSnapshot == "false""#)
    );
}

#[test]
fn test_unset_clears_key_from_both_maps() {
    let mut vars = TemplateVars::new();

    // Structured-owned key: unset must remove it and report presence.
    vars.set_bool("IsSnapshot", true);
    assert!(vars.unset("IsSnapshot"));
    assert!(vars.get("IsSnapshot").is_none());
    assert!(vars.get_structured("IsSnapshot").is_none());

    // String-owned key: unset_structured clears it too (both removers
    // uphold the one-map-per-key invariant).
    vars.set("Tag", "v1.0.0");
    assert!(vars.unset_structured("Tag"));
    assert!(vars.get("Tag").is_none());
    assert!(vars.get_structured("Tag").is_none());

    // Absent key: both report false.
    assert!(!vars.unset("Missing"));
    assert!(!vars.unset_structured("Missing"));
}
