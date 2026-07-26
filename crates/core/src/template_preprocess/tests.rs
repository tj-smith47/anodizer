//! Tests for the template preprocessor.

use super::preprocess;

/// Force-touch every `LazyLock<Regex>` static in the preprocessor so an
/// invalid literal surfaces here, not on the first real preprocess() call
/// in the field. Each `LazyLock::new(|| static_regex(…))` panics on bad
/// pattern; running them under the test binary turns a deferred panic into
/// a deterministic test failure.
#[test]
fn static_regex_literals_compile() {
    let _ = preprocess("{{ Version }}");
    let _ = preprocess("{{ replace Version \"v\" \"\" }}");
    let _ = preprocess("{{ Version | replace \"v\" \"\" }}");
    let _ = preprocess("{{ in (list \"a\" \"b\") \"a\" }}");
    let _ = preprocess("{{ Now.Format \"2006\" }}");
    let _ = preprocess("{% if eq .Os \"linux\" %}x{% end %}");
    let _ = preprocess("{{ map \"k1\" \"v1\" }}");
}

#[test]
fn test_preprocess_positional_replace() {
    // Unit test for the preprocessor output
    let input = "{{ replace Version \"v\" \"\" }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ replace(s=Version, old=\"v\", new=\"\") }}");
}

#[test]
fn test_preprocess_positional_replace_piped() {
    let input = "{{ Version | replace \"v\" \"\" }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ Version | replace(from=\"v\", to=\"\") }}");
}

#[test]
fn test_preprocess_positional_split() {
    let input = "{{ split Version \".\" }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ split(s=Version, sep=\".\") }}");
}

#[test]
fn test_preprocess_positional_contains() {
    let input = "{{ contains Version \"rc\" }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ contains(s=Version, substr=\"rc\") }}");
}

#[test]
fn test_preprocess_positional_piped_split() {
    let input = "{{ Version | split \".\" }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ Version | split(sep=\".\") }}");
}

#[test]
fn test_preprocess_positional_piped_contains() {
    let input = "{{ Version | contains \"rc\" }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ Version | contains(substr=\"rc\") }}");
}

#[test]
fn test_preprocess_named_args_unchanged() {
    // Already-named-arg syntax should pass through unmodified
    let input = "{{ replace(s=Version, old=\"v\", new=\"\") }}";
    let result = preprocess(input);
    assert_eq!(result, input);
}

#[test]
fn test_preprocess_named_filter_unchanged() {
    let input = "{{ Version | replace(from=\"v\", to=\"\") }}";
    let result = preprocess(input);
    assert_eq!(result, input);
}

#[test]
fn test_preprocess_control_block_rewritten() {
    // {% if contains Version "rc" %} should be rewritten to named-arg form
    let input = "{% if contains Version \"rc\" %}yes{% endif %}";
    let result = preprocess(input);
    assert_eq!(
        result,
        "{% if contains(s=Version, substr=\"rc\") %}yes{% endif %}"
    );
}

#[test]
fn test_preprocess_control_block_non_positional_unchanged() {
    // {% if Version %} should not be touched (no positional func)
    let input = "{% if Version %}yes{% endif %}";
    let result = preprocess(input);
    assert_eq!(result, input);
}

#[test]
fn test_positional_replace_with_dot_var() {
    // Dot-stripping + positional rewrite combined:
    // {{ replace .Tag "v" "" }} → dot-strip → {{ replace Tag "v" "" }} → positional → {{ replace(s=Tag, old="v", new="") }}
    let input = "{{ replace .Tag \"v\" \"\" }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ replace(s=Tag, old=\"v\", new=\"\") }}");
}

#[test]
fn test_positional_piped_with_dot_var() {
    // {{ .Tag | replace "v" "" }} → dot-strip → {{ Tag | replace "v" "" }} → positional
    let input = "{{ .Tag | replace \"v\" \"\" }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ Tag | replace(from=\"v\", to=\"\") }}");
}

#[test]
fn test_positional_no_spaces_compact() {
    // Compact form: {{replace .Tag "v" ""}}
    let input = "{{replace .Tag \"v\" \"\"}}";
    let result = preprocess(input);
    assert_eq!(result, "{{replace(s=Tag, old=\"v\", new=\"\")}}");
}

#[test]
fn test_optional_chaining_dot_survives_go_leading_dot_strip() {
    // `?.` is tera 2.0's native optional-chaining operator, lexed as one
    // token. The Go-leading-dot-strip pass used to treat the `.` here the
    // same as a Go `{{ .Field }}` leading dot (since `?` isn't a word
    // char) and strip it, corrupting `Some?.Missing` into the
    // parse-error `Some?Missing`. A `?` immediately before the dot must
    // count as "chained access" like a preceding identifier does.
    let input = "{{ Some?.Missing or \"fallback\" }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ Some?.Missing or \"fallback\" }}");
}

#[test]
fn test_unrelated_expression_unchanged() {
    // A simple variable reference should not be affected
    let input = "{{ Version }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ Version }}");
}

#[test]
fn test_unrelated_filter_unchanged() {
    // A normal filter chain should not be affected
    let input = "{{ Version | upper }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ Version | upper }}");
}

#[test]
fn test_positional_replace_whitespace_control() {
    // Tera whitespace control: {{- and -}}
    let input = "{{- replace Version \"v\" \"\" -}}";
    let result = preprocess(input);
    assert_eq!(result, "{{- replace(s=Version, old=\"v\", new=\"\") -}}");
}

#[test]
fn test_positional_replace_whitespace_control_left_only() {
    let input = "{{- replace Version \"v\" \"\" }}";
    let result = preprocess(input);
    assert_eq!(result, "{{- replace(s=Version, old=\"v\", new=\"\") }}");
}

#[test]
fn test_chained_named_filter_then_positional_rewrite() {
    // Chained: named-arg filter followed by positional rewrite.
    // The preprocessor should rewrite ONLY the last segment's positional args.
    let input = "{{ Version | trimprefix(prefix=\"v\") | replace \".\" \"-\" }}";
    let result = preprocess(input);
    assert_eq!(
        result,
        "{{ Version | trimprefix(prefix=\"v\") | replace(from=\".\", to=\"-\") }}"
    );
}

// --- `in` positional syntax preprocessing tests ---

#[test]
fn test_preprocess_in_with_list_subexpr() {
    // Go-style: {{ in (list "a" "b" "c") "b" }}
    // Pass 2: (list "a" "b" "c") → ["a", "b", "c"]
    // Pass 3: in ["a", "b", "c"] "b" → in(items=["a", "b", "c"], value="b")
    let input = "{{ in (list \"a\" \"b\" \"c\") \"b\" }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ in(items=[\"a\", \"b\", \"c\"], value=\"b\") }}");
}

#[test]
fn test_preprocess_in_with_variable() {
    // Positional: {{ in myList "b" }} → {{ in(items=myList, value="b") }}
    let input = "{{ in myList \"b\" }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ in(items=myList, value=\"b\") }}");
}

#[test]
fn test_preprocess_in_named_args_unchanged() {
    let input = "{{ in(items=[\"a\", \"b\"], value=\"a\") }}";
    let result = preprocess(input);
    assert_eq!(result, input);
}

#[test]
fn test_preprocess_in_with_dot_var() {
    // {{ in .MyList "val" }} → dot-strip → {{ in MyList "val" }} → positional
    let input = "{{ in .MyList \"val\" }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ in(items=MyList, value=\"val\") }}");
}

#[test]
fn test_preprocess_in_control_block() {
    // {% if in myList "b" %} → {% if in(items=myList, value="b") %}
    let input = "{% if in myList \"b\" %}yes{% endif %}";
    let result = preprocess(input);
    assert_eq!(
        result,
        "{% if in(items=myList, value=\"b\") %}yes{% endif %}"
    );
}

#[test]
fn test_preprocess_list_subexpr_rewrite() {
    // Verify the list subexpression rewrite pass in isolation:
    // (list "a" "b" "c") → ["a", "b", "c"]
    let input = "{{ in (list \"x\" \"y\") \"x\" }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ in(items=[\"x\", \"y\"], value=\"x\") }}");
}

#[test]
fn test_preprocess_in_control_block_with_list_subexpr() {
    // {% if in (list "a" "b") "a" %} → list rewrite → {% if in ["a", "b"] "a" %}
    // → positional → {% if in(items=["a", "b"], value="a") %}
    let input = "{% if in (list \"a\" \"b\") \"a\" %}yes{% endif %}";
    let result = preprocess(input);
    assert_eq!(
        result,
        "{% if in(items=[\"a\", \"b\"], value=\"a\") %}yes{% endif %}"
    );
}

// --- `reReplaceAll` positional syntax preprocessing tests ---

#[test]
fn test_preprocess_re_replace_all_positional() {
    // {{ reReplaceAll "(.*)" "hello" "$1-world" }}
    // → {{ reReplaceAll(pattern="(.*)", input="hello", replacement="$1-world") }}
    let input = "{{ reReplaceAll \"(.*)\" \"hello\" \"$1-world\" }}";
    let result = preprocess(input);
    assert_eq!(
        result,
        "{{ reReplaceAll(pattern=\"(.*)\", input=\"hello\", replacement=\"$1-world\") }}"
    );
}

#[test]
fn test_preprocess_re_replace_all_with_variable() {
    // {{ reReplaceAll "(v)(.*)" Tag "prefix-$2" }}
    // → {{ reReplaceAll(pattern="(v)(.*)", input=Tag, replacement="prefix-$2") }}
    let input = "{{ reReplaceAll \"(v)(.*)\" Tag \"prefix-$2\" }}";
    let result = preprocess(input);
    assert_eq!(
        result,
        "{{ reReplaceAll(pattern=\"(v)(.*)\", input=Tag, replacement=\"prefix-$2\") }}"
    );
}

#[test]
fn test_preprocess_re_replace_all_named_args_unchanged() {
    let input = "{{ reReplaceAll(pattern=\"x\", input=\"ax\", replacement=\"y\") }}";
    let result = preprocess(input);
    assert_eq!(result, input);
}

#[test]
fn test_preprocess_re_replace_all_piped() {
    // {{ Message | reReplaceAll "(.*)" "$1-done" }}
    // → {{ Message | reReplaceAll(pattern="(.*)", replacement="$1-done") }}
    let input = "{{ Message | reReplaceAll \"(.*)\" \"$1-done\" }}";
    let result = preprocess(input);
    assert_eq!(
        result,
        "{{ Message | reReplaceAll(pattern=\"(.*)\", replacement=\"$1-done\") }}"
    );
}

#[test]
fn test_preprocess_re_replace_all_control_block() {
    // {% if reReplaceAll "v" Tag "" %} → named-arg form
    let input = "{% if reReplaceAll \"v\" Tag \"\" %}yes{% endif %}";
    let result = preprocess(input);
    assert_eq!(
        result,
        "{% if reReplaceAll(pattern=\"v\", input=Tag, replacement=\"\") %}yes{% endif %}"
    );
}

// --- `in` piped form preprocessing tests ---

#[test]
fn test_preprocess_in_piped() {
    // {{ myList | in "val" }} → {{ myList | in(value="val") }}
    let input = "{{ myList | in \"val\" }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ myList | in(value=\"val\") }}");
}

// --- list subexpr: escaped quotes and mixed quote styles ---

#[test]
fn test_preprocess_list_subexpr_escaped_double_quotes_error_loudly() {
    // Under the engine's raw string rule `"hello \"` closes at the quote
    // right after the backslash — `\"` never embedded a quote, so this
    // template was never valid. The sub-expression is passed through
    // byte-for-byte (only the enclosing `in` call is rewritten), so the engine
    // errors loudly instead of the pass silently reinterpreting the boundaries.
    use crate::template::{TemplateVars, render};
    let input = r#"{{ in (list "hello \"world\"" "plain") "plain" }}"#;
    assert_eq!(
        preprocess(input),
        r#"{{ in(items=(list "hello \"world\"" "plain"), value="plain") }}"#
    );
    assert!(render(input, &TemplateVars::new()).is_err());
}

#[test]
fn test_preprocess_list_subexpr_escaped_single_quotes_error_loudly() {
    // Same raw-rule reality for single quotes: `'it\'` closes after the
    // backslash, leaving a dangling `s'` — loud error, no silent rewrite.
    use crate::template::{TemplateVars, render};
    let input = "{{ in (list 'it\\'s' 'fine') \"fine\" }}";
    assert_eq!(preprocess(input), input);
    assert!(render(input, &TemplateVars::new()).is_err());
}

#[test]
fn test_preprocess_list_subexpr_mixed_quote_styles() {
    // (list "double" 'single' "another") — each item uses its own quote style
    let input = "{{ in (list \"double\" 'single' \"another\") \"double\" }}";
    let result = preprocess(input);
    assert_eq!(
        result,
        "{{ in(items=[\"double\", 'single', \"another\"], value=\"double\") }}"
    );
}

// --- Finding 5: `(list ...)` with bare identifiers (variable references) ---

#[test]
fn test_preprocess_list_subexpr_with_bare_identifier() {
    // (list .Os "windows") → after dot-strip: (list Os "windows") → [Os, "windows"]
    let input = "{{ in (list .Os \"windows\") \"linux\" }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ in(items=[Os, \"windows\"], value=\"linux\") }}");
}

#[test]
fn test_preprocess_list_subexpr_with_dotted_path() {
    // (list .Env.FOO "fallback") → after dot-strip: (list Env.FOO "fallback") → [Env.FOO, "fallback"]
    let input = "{{ in (list .Env.FOO \"fallback\") \"val\" }}";
    let result = preprocess(input);
    assert_eq!(
        result,
        "{{ in(items=[Env.FOO, \"fallback\"], value=\"val\") }}"
    );
}

#[test]
fn test_preprocess_list_subexpr_all_bare_identifiers() {
    // (list .Os .Arch) → after dot-strip: (list Os Arch) → [Os, Arch]
    let input = "{{ in (list .Os .Arch) \"linux\" }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ in(items=[Os, Arch], value=\"linux\") }}");
}

#[test]
fn test_preprocess_list_subexpr_mixed_vars_and_strings() {
    // (list .Os "windows" .Arch) → after dot-strip: (list Os "windows" Arch) → [Os, "windows", Arch]
    let input = "{{ in (list .Os \"windows\" .Arch) \"test\" }}";
    let result = preprocess(input);
    assert_eq!(
        result,
        "{{ in(items=[Os, \"windows\", Arch], value=\"test\") }}"
    );
}

// --- Now.Format method call rewrite tests ---

#[test]
fn test_preprocess_now_format_go_style() {
    // {{ .Now.Format "2006-01-02" }} → {{ Now | now_format(format="2006-01-02") }}
    let input = "{{ .Now.Format \"2006-01-02\" }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ Now | now_format(format=\"2006-01-02\") }}");
}

#[test]
fn test_preprocess_now_format_no_dot_prefix() {
    // {{ Now.Format "2006-01-02" }} (without leading dot) should also work
    let input = "{{ Now.Format \"2006-01-02\" }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ Now | now_format(format=\"2006-01-02\") }}");
}

#[test]
fn test_preprocess_now_format_with_time_pattern() {
    // {{ .Now.Format "2006-01-02 15:04:05" }}
    let input = "{{ .Now.Format \"2006-01-02 15:04:05\" }}";
    let result = preprocess(input);
    assert_eq!(
        result,
        "{{ Now | now_format(format=\"2006-01-02 15:04:05\") }}"
    );
}

#[test]
fn test_preprocess_now_format_single_quotes() {
    // {{ .Now.Format '2006-01-02' }} (single quotes)
    let input = "{{ .Now.Format '2006-01-02' }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ Now | now_format(format='2006-01-02') }}");
}

#[test]
fn test_preprocess_now_format_whitespace_control() {
    // {{- .Now.Format "2006-01-02" -}}
    let input = "{{- .Now.Format \"2006-01-02\" -}}";
    let result = preprocess(input);
    assert_eq!(result, "{{- Now | now_format(format=\"2006-01-02\") -}}");
}

#[test]
fn test_preprocess_now_format_compact() {
    // {{.Now.Format "2006-01-02"}} (no spaces after {{ or before }})
    let input = "{{.Now.Format \"2006-01-02\"}}";
    let result = preprocess(input);
    assert_eq!(result, "{{Now | now_format(format=\"2006-01-02\")}}");
}

#[test]
fn test_preprocess_now_format_does_not_affect_other_blocks() {
    // Other blocks should not be affected
    let input = "{{ Version }} - {{ .Now.Format \"2006-01-02\" }}";
    let result = preprocess(input);
    assert_eq!(
        result,
        "{{ Version }} - {{ Now | now_format(format=\"2006-01-02\") }}"
    );
}

// -----------------------------------------------------------------------
// Pass 0: Go block syntax tests
// -----------------------------------------------------------------------

#[test]
fn test_go_if_end() {
    let input = "{{ if .IsSnapshot }}pre{{ end }}";
    let result = preprocess(input);
    assert_eq!(result, "{% if IsSnapshot %}pre{% endif %}");
}

#[test]
fn test_go_if_else_end() {
    let input = "{{ if .IsSnapshot }}pre{{ else }}stable{{ end }}";
    let result = preprocess(input);
    assert_eq!(result, "{% if IsSnapshot %}pre{% else %}stable{% endif %}");
}

#[test]
fn test_go_if_else_if_end() {
    let input =
        "{{ if eq .Os \"windows\" }}win{{ else if eq .Os \"darwin\" }}mac{{ else }}linux{{ end }}";
    let result = preprocess(input);
    // `eq Os "windows"` is rewritten to `Os == "windows"` by Pass 2b
    assert_eq!(
        result,
        "{% if Os == \"windows\" %}win{% elif Os == \"darwin\" %}mac{% else %}linux{% endif %}"
    );
}

#[test]
fn test_go_range_bare() {
    let input = "{{ range .Maintainers }}# {{ . }}{{ end }}";
    let result = preprocess(input);
    assert_eq!(
        result,
        "{% for val in Maintainers %}# {{ val }}{% endfor %}"
    );
}

#[test]
fn test_go_range_with_variable() {
    let input = "{{ range $release := .Packages }}{{ $release.Name }}{{ end }}";
    let result = preprocess(input);
    assert_eq!(
        result,
        "{% for release in Packages %}{{ release.Name }}{% endfor %}"
    );
}

#[test]
fn test_go_range_kv() {
    let input = "{{ range $key, $value := .Checksums }}{{ $value }} {{ $key }}{{ end }}";
    let result = preprocess(input);
    assert_eq!(
        result,
        "{% for key, value in Checksums %}{{ value }} {{ key }}{% endfor %}"
    );
}

#[test]
fn test_go_with() {
    let input = "{{ with .Arm }}v{{ . }}{{ end }}";
    let result = preprocess(input);
    // `with` becomes `if`, `{{ . }}` rewrites to the with argument
    assert_eq!(result, "{% if Arm %}v{{ Arm }}{% endif %}");
}

#[test]
fn test_go_var_assignment() {
    let input = "{{ $m := map \"a\" \"1\" }}{{ index $m \"a\" }}";
    let result = preprocess(input);
    // Pass 2c rewrites `map "a" "1"` to `map(pairs=["a", "1"])`
    // Pass 3 rewrites `index m "a"` to `index(collection=m, key="a")`
    assert_eq!(
        result,
        "{% set m = map(pairs=[\"a\", \"1\"]) %}{{ index(collection=m, key=\"a\") }}"
    );
}

#[test]
fn test_go_whitespace_trim() {
    let input = "{{- if .Cond -}}yes{{- end -}}";
    let result = preprocess(input);
    assert_eq!(result, "{%- if Cond -%}yes{%- endif -%}");
}

#[test]
fn test_go_nested_if_range() {
    let input = "{{ range .Items }}{{ if .Active }}*{{ end }}{{ end }}";
    let result = preprocess(input);
    assert_eq!(
        result,
        "{% for val in Items %}{% if Active %}*{% endif %}{% endfor %}"
    );
}

#[test]
fn test_go_blocks_plain_expressions_unchanged() {
    // Plain Go expressions (no block keywords) should pass through
    let input = "{{ .ProjectName }}_{{ .Version }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ ProjectName }}_{{ Version }}");
}

#[test]
fn test_go_complex_nfpm_template() {
    // Real-world template: nfpm default name_template
    let input = "{{ .ProjectName }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}{{ with .Arm }}v{{ . }}{{ end }}{{ if not (eq .Amd64 \"v1\") }}{{ .Amd64 }}{{ end }}";
    let result = preprocess(input);
    // `(eq Amd64 "v1")` is rewritten to `Amd64 == "v1"` by Pass 2b
    // Parens are stripped because Tera doesn't support comparisons inside parens.
    assert_eq!(
        result,
        "{{ ProjectName }}_{{ Version }}_{{ Os }}_{{ Arch }}{% if Arm %}v{{ Arm }}{% endif %}{% if not Amd64 == \"v1\" %}{{ Amd64 }}{% endif %}"
    );
}

// -----------------------------------------------------------------------
// Pass 2b: comparison functions (eq/ne/gt/lt/ge/le), and/or, len
// -----------------------------------------------------------------------

#[test]
fn test_eq_in_if_block() {
    let input = "{% if eq Os \"windows\" %}win{% endif %}";
    let result = preprocess(input);
    assert_eq!(result, "{% if Os == \"windows\" %}win{% endif %}");
}

#[test]
fn test_eq_variadic_three_args() {
    // Go's eq is variadic: eq X Y Z means X == Y || X == Z
    let input = r#"{% if eq Os "linux" "darwin" %}unix{% endif %}"#;
    let result = preprocess(input);
    assert_eq!(
        result,
        r#"{% if Os == "linux" or Os == "darwin" %}unix{% endif %}"#
    );
}

#[test]
fn test_eq_variadic_four_args() {
    let input = r#"{% if eq Arch "amd64" "arm64" "386" %}supported{% endif %}"#;
    let result = preprocess(input);
    assert_eq!(
        result,
        r#"{% if Arch == "amd64" or Arch == "arm64" or Arch == "386" %}supported{% endif %}"#
    );
}

#[test]
fn test_ne_in_if_block() {
    let input = "{% if ne Os \"windows\" %}not-win{% endif %}";
    let result = preprocess(input);
    assert_eq!(result, "{% if Os != \"windows\" %}not-win{% endif %}");
}

#[test]
fn test_gt_in_if_block() {
    let input = "{% if gt Major 1 %}gt1{% endif %}";
    let result = preprocess(input);
    assert_eq!(result, "{% if Major > 1 %}gt1{% endif %}");
}

#[test]
fn test_lt_in_if_block() {
    let input = "{% if lt Minor 5 %}lt5{% endif %}";
    let result = preprocess(input);
    assert_eq!(result, "{% if Minor < 5 %}lt5{% endif %}");
}

#[test]
fn test_ge_in_if_block() {
    let input = "{% if ge Patch 3 %}ge3{% endif %}";
    let result = preprocess(input);
    assert_eq!(result, "{% if Patch >= 3 %}ge3{% endif %}");
}

#[test]
fn test_le_in_if_block() {
    let input = "{% if le Patch 3 %}le3{% endif %}";
    let result = preprocess(input);
    assert_eq!(result, "{% if Patch <= 3 %}le3{% endif %}");
}

#[test]
fn test_eq_with_string_literal() {
    let input = "{% if eq Arch \"amd64\" %}yes{% endif %}";
    let result = preprocess(input);
    assert_eq!(result, "{% if Arch == \"amd64\" %}yes{% endif %}");
}

#[test]
fn test_eq_with_numeric_literal() {
    let input = "{% if eq Major 1 %}v1{% endif %}";
    let result = preprocess(input);
    assert_eq!(result, "{% if Major == 1 %}v1{% endif %}");
}

#[test]
fn test_eq_parenthesized_not() {
    // not (eq .Os "windows") → not Os == "windows"
    // Tera doesn't support comparison operators inside parens, so parens are stripped.
    let input = "{% if not (eq Os \"windows\") %}yes{% endif %}";
    let result = preprocess(input);
    assert_eq!(result, "{% if not Os == \"windows\" %}yes{% endif %}");
}

#[test]
fn test_eq_in_elif_block() {
    let input = "{% if eq Os \"linux\" %}lin{% elif eq Os \"darwin\" %}mac{% endif %}";
    let result = preprocess(input);
    assert_eq!(
        result,
        "{% if Os == \"linux\" %}lin{% elif Os == \"darwin\" %}mac{% endif %}"
    );
}

#[test]
fn test_eq_in_expression_block() {
    // eq can also appear in {{ }} expression blocks
    let input = "{{ eq Os \"linux\" }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ Os == \"linux\" }}");
}

#[test]
fn test_eq_with_already_stripped_dot_var() {
    // After dot stripping: eq Os "windows"
    let input = "{% if eq Os \"windows\" %}win{% endif %}";
    let result = preprocess(input);
    assert_eq!(result, "{% if Os == \"windows\" %}win{% endif %}");
}

#[test]
fn test_eq_with_dotted_path() {
    // eq Env.FOO "bar"
    let input = "{% if eq Env.FOO \"bar\" %}yes{% endif %}";
    let result = preprocess(input);
    assert_eq!(result, "{% if Env.FOO == \"bar\" %}yes{% endif %}");
}

// --- and/or prefix to infix ---

#[test]
fn test_and_prefix_to_infix() {
    let input = "{% if and A B %}yes{% endif %}";
    let result = preprocess(input);
    assert_eq!(result, "{% if A and B %}yes{% endif %}");
}

#[test]
fn test_or_prefix_to_infix() {
    let input = "{% if or A B %}yes{% endif %}";
    let result = preprocess(input);
    assert_eq!(result, "{% if A or B %}yes{% endif %}");
}

#[test]
fn test_and_with_parenthesized_or() {
    // and .A (or .B .C) → A and (B or C)
    let input = "{% if and A (or B C) %}yes{% endif %}";
    let result = preprocess(input);
    assert_eq!(result, "{% if A and (B or C) %}yes{% endif %}");
}

#[test]
fn test_or_with_parenthesized_eq() {
    // or (eq Os "linux") (eq Os "darwin") → Os == "linux" or Os == "darwin"
    // Tera doesn't support comparisons inside parens, so all parens are stripped.
    let input = "{% if or (eq Os \"linux\") (eq Os \"darwin\") %}yes{% endif %}";
    let result = preprocess(input);
    assert_eq!(
        result,
        "{% if Os == \"linux\" or Os == \"darwin\" %}yes{% endif %}"
    );
}

// --- len function ---

#[test]
fn test_len_in_expression() {
    let input = "{{ len Items }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ Items | length }}");
}

#[test]
fn test_len_in_if_block() {
    let input = "{% if len Items %}has items{% endif %}";
    let result = preprocess(input);
    assert_eq!(result, "{% if Items | length %}has items{% endif %}");
}

#[test]
fn test_len_with_dotted_path() {
    let input = "{{ len Env.PATH }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ Env.PATH | length }}");
}

#[test]
fn test_len_does_not_match_partial_word() {
    // "length" should not be rewritten
    let input = "{{ Items | length }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ Items | length }}");
}

// --- map positional syntax ---

#[test]
fn test_map_positional_two_args() {
    let input = "{{ map \"a\" \"1\" }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ map(pairs=[\"a\", \"1\"]) }}");
}

#[test]
fn test_map_positional_four_args() {
    let input = "{{ map \"a\" \"1\" \"b\" \"2\" }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ map(pairs=[\"a\", \"1\", \"b\", \"2\"]) }}");
}

#[test]
fn test_map_named_args_unchanged() {
    let input = "{{ map(pairs=[\"a\", \"1\"]) }}";
    let result = preprocess(input);
    assert_eq!(result, input);
}

#[test]
fn test_map_in_set_block() {
    let input = "{% set m = map \"x\" \"y\" %}";
    let result = preprocess(input);
    assert_eq!(result, "{% set m = map(pairs=[\"x\", \"y\"]) %}");
}

// --- index positional syntax ---

#[test]
fn test_index_positional_two_args() {
    let input = "{{ index myMap \"key\" }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ index(collection=myMap, key=\"key\") }}");
}

#[test]
fn test_index_named_args_unchanged() {
    let input = "{{ index(collection=myMap, key=\"key\") }}";
    let result = preprocess(input);
    assert_eq!(result, input);
}

#[test]
fn test_index_in_control_block() {
    let input = "{% if index myMap \"key\" %}yes{% endif %}";
    let result = preprocess(input);
    assert_eq!(
        result,
        "{% if index(collection=myMap, key=\"key\") %}yes{% endif %}"
    );
}

// --- Combined pass tests ---

#[test]
fn test_go_style_full_pipeline_eq_and_map() {
    // Full Go-style pipeline:
    // {{ $m := map "a" "1" }}{{ if eq (index $m "a") "1" }}yes{{ end }}
    let input = "{{ $m := map \"a\" \"1\" }}{{ if eq (index $m \"a\") \"1\" }}yes{{ end }}";
    let result = preprocess(input);
    // Pass 2b rewrites `eq (index m "a") "1"` to `(index m "a") == "1"`.
    // Parens around `index m "a"` are kept (no comparison operator inside).
    // Pass 2c rewrites `map "a" "1"` to `map(pairs=["a", "1"])`.
    // Pass 3 then descends into the surviving sub-expression, so the `index`
    // call inside the parens reaches its named-arg form too.
    assert_eq!(
        result,
        "{% set m = map(pairs=[\"a\", \"1\"]) %}\
         {% if (index(collection=m, key=\"a\")) == \"1\" %}yes{% endif %}"
    );
}

#[test]
fn test_preprocess_positional_time() {
    let input = "{{ time \"2006-01-02\" }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ time(format=\"2006-01-02\") }}");
}

#[test]
fn test_preprocess_slice_three_args() {
    let input = "{{ slice Commit 0 7 }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ Commit | slice(start=0, end=7) }}");
}

#[test]
fn test_preprocess_slice_two_args() {
    let input = "{{ slice Commit 0 }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ Commit | slice(start=0) }}");
}

#[test]
fn test_preprocess_slice_string_literal() {
    let input = "{{ slice \"abcdefghij\" 0 7 }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ \"abcdefghij\" | slice(start=0, end=7) }}");
}

#[test]
fn test_preprocess_slice_named_unchanged() {
    let input = "{{ Commit | slice(start=0, end=7) }}";
    let result = preprocess(input);
    assert_eq!(result, input);
}

#[test]
fn test_preprocess_printf_variadic() {
    let input = "{{ printf \"%04d\" Patch }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ printf(format=\"%04d\", args=[Patch]) }}");
}

#[test]
fn test_preprocess_printf_multiple_args() {
    let input = "{{ printf \"%s-%d\" Os Patch }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ printf(format=\"%s-%d\", args=[Os, Patch]) }}");
}

#[test]
fn test_preprocess_printf_no_args() {
    let input = "{{ printf \"literal\" }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ printf(format=\"literal\", args=[]) }}");
}

#[test]
fn test_preprocess_print() {
    let input = "{{ print \"a\" \"b\" }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ print(args=[\"a\", \"b\"]) }}");
}

#[test]
fn test_preprocess_println() {
    let input = "{{ println \"x\" }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ println(args=[\"x\"]) }}");
}

#[test]
fn test_preprocess_printf_named_unchanged() {
    let input = "{{ printf(format=\"%d\", args=[Patch]) }}";
    let result = preprocess(input);
    assert_eq!(result, input);
}

#[test]
fn test_preprocess_preserves_emoji_in_literal_text() {
    // A non-ASCII char in plain literal text (no template syntax) must survive
    // the byte-walk intact, not get Latin-1-decoded into mojibake.
    let input = "Released with anodizer 🦀";
    let result = preprocess(input);
    assert!(result.contains('🦀'), "emoji must survive, got: {result:?}");
    assert_eq!(result, input);
}

#[test]
fn test_preprocess_preserves_multibyte_mix_in_literal_text() {
    // Mixed multibyte literal (accents, CJK, emoji, em-dash) round-trips unchanged.
    let input = "café — 日本語 🚀 end";
    let result = preprocess(input);
    assert_eq!(result, input);
}

#[test]
fn test_preprocess_preserves_emoji_inside_block_string() {
    // A non-ASCII char inside a quoted block string must survive the
    // dots_dollars/strip_dots byte-walk that copies quoted-string content.
    let input = "{{ printf \"%s\" \"🦀\" }}";
    let result = preprocess(input);
    assert!(
        result.contains('🦀'),
        "emoji inside block string must survive, got: {result:?}"
    );
}

#[test]
fn test_optional_index_dot_survives_go_leading_dot_strip() {
    // `?[` is tera 2.0's optional-index operator, the sibling of `?.`
    // (same lexer family: exactly two `?` tokens exist). A `.` immediately
    // after the `]` that closes `Some?[0]` is chained field access
    // (`Some?[0].Field`), not a Go-style leading dot — stripping it would
    // corrupt the template into the parse error `Some?[0]Field`.
    let input = "{{ Some?[0].Field or \"fallback\" }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ Some?[0].Field or \"fallback\" }}");
}

#[test]
fn test_plain_index_dot_survives_go_leading_dot_strip() {
    // Pins existing (now-correct) behavior for plain, non-optional
    // indexing: `Some[0].Field` is chained field access after an index,
    // same reasoning as the `?[` case above but without the `?`.
    let input = "{{ Some[0].Field }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ Some[0].Field }}");
}

// ---- Pass 5: tera 1.x numeric-index compatibility (`list.0` → `list[0]`) ----

#[test]
fn test_numeric_index_simple() {
    assert_eq!(preprocess("{{ list.0 }}"), "{{ list[0] }}");
}

#[test]
fn test_numeric_index_multi_digit() {
    assert_eq!(preprocess("{{ list.12 }}"), "{{ list[12] }}");
}

#[test]
fn test_numeric_index_then_field() {
    assert_eq!(preprocess("{{ a.0.b }}"), "{{ a[0].b }}");
}

#[test]
fn test_numeric_index_chained_indices() {
    assert_eq!(preprocess("{{ a.0.1 }}"), "{{ a[0][1] }}");
}

#[test]
fn test_numeric_index_after_bracket_index() {
    assert_eq!(preprocess("{{ x[1].0 }}"), "{{ x[1][0] }}");
}

#[test]
fn test_numeric_index_optional_chaining() {
    // tera 2.0 lexes `?[` as its optional-index token (the `?.` sibling),
    // so the 1.x-era `a?.0` must land on `a?[0]`.
    assert_eq!(preprocess("{{ a?.0 }}"), "{{ a?[0] }}");
}

#[test]
fn test_numeric_index_go_style_leading_dot() {
    assert_eq!(
        preprocess("{{ .Artifacts.0.Name }}"),
        "{{ Artifacts[0].Name }}"
    );
}

#[test]
fn test_numeric_index_in_statement_block() {
    assert_eq!(
        preprocess("{% if list.0 %}x{% endif %}"),
        "{% if list[0] %}x{% endif %}"
    );
}

#[test]
fn test_numeric_index_float_literal_untouched() {
    // A number literal is not a path head — `1.0` stays a float.
    assert_eq!(preprocess("{{ 1.0 }}"), "{{ 1.0 }}");
    assert_eq!(preprocess("{{ 1.5 | round }}"), "{{ 1.5 | round }}");
}

#[test]
fn test_numeric_index_version_literal_untouched() {
    // Chained digits-only segments are float/version-shaped literals,
    // never paths.
    assert_eq!(preprocess("{{ 1.2 + 0.5 }}"), "{{ 1.2 + 0.5 }}");
}

#[test]
fn test_numeric_index_inside_string_literal_untouched() {
    assert_eq!(preprocess("{{ \"a.0\" }}"), "{{ \"a.0\" }}");
    assert_eq!(
        preprocess("{{ foo | replace(from=\"v1.0\", to=\"\") }}"),
        "{{ foo | replace(from=\"v1.0\", to=\"\") }}"
    );
}

#[test]
fn test_numeric_index_identifierish_segment_untouched() {
    // `.0x` is not a pure numeric segment; leave it for tera's parser
    // to diagnose.
    assert_eq!(preprocess("{{ a.0x }}"), "{{ a.0x }}");
}

#[test]
fn test_numeric_index_outside_blocks_untouched() {
    // Only expression blocks are rewritten; literal text keeps its dots.
    assert_eq!(preprocess("v1.0 of {{ name }}"), "v1.0 of {{ name }}");
}

#[test]
fn test_numeric_index_end_to_end_render() {
    // Proves `{{ list.0 }}` renders under tera 2.0 via the public API,
    // not just that the rewrite produces the expected text.
    use crate::template::{TemplateVars, render};
    let mut vars = TemplateVars::new();
    vars.set_structured("list", serde_json::json!(["first", "second"]));
    vars.set_structured("items", serde_json::json!([{"name": "n0"}]));
    assert_eq!(render("{{ list.0 }}", &vars).unwrap(), "first");
    assert_eq!(render("{{ items.0.name }}", &vars).unwrap(), "n0");
    assert_eq!(render("{{ list.1 | upper }}", &vars).unwrap(), "SECOND");
}

// --- raw string-boundary rule (shared `string_lit` helper) ---
// All passes must close string literals exactly where the engine does:
// first next occurrence of the opening delimiter (`"`, `'`, or backtick),
// no escape awareness.

#[test]
fn test_backtick_string_skipped_by_numeric_index_pass() {
    assert_eq!(preprocess("{{ `v1.0` }}"), "{{ `v1.0` }}");
}

#[test]
fn test_backtick_string_skipped_by_dollar_strip() {
    assert_eq!(preprocess("{{ `$foo` }}"), "{{ `$foo` }}");
}

#[test]
fn test_backtick_string_skipped_by_dot_strip() {
    assert_eq!(preprocess("{{ `.Field` }}"), "{{ `.Field` }}");
}

#[test]
fn test_backtick_filter_arg_content_untouched() {
    let input = "{{ 'v1.0-x' | replace(from=`v1.0`, to=\"Z\") }}";
    assert_eq!(preprocess(input), input);
}

#[test]
fn test_string_closes_at_first_quote_even_after_backslash() {
    // `'Q\'` closes at the second quote (the engine has no escape concept),
    // so `'v1.0'` is a STRING whose `.0` must survive verbatim.
    let input = r"{{ 'Q\' ~ 'v1.0' }}";
    assert_eq!(preprocess(input), input);
}

// --- multiline expression blocks receive every pass ---

#[test]
fn test_multiline_block_dot_strip_applies() {
    assert_eq!(preprocess("{{\n  .Version }}"), "{{\n  Version }}");
}

#[test]
fn test_multiline_block_dollar_strip_applies() {
    assert_eq!(preprocess("{{\n  $foo }}"), "{{\n  foo }}");
}

#[test]
fn test_multiline_block_builtin_rewrite_applies() {
    assert_eq!(
        preprocess("{% if eq Os \"linux\"\n%}yes{% endif %}"),
        "{% if Os == \"linux\"\n%}yes{% endif %}"
    );
}

#[test]
fn test_multiline_block_method_call_rewrite_applies() {
    assert_eq!(
        preprocess("{{\n  .Now.Format \"2006-01-02\" }}"),
        "{{\n  Now | now_format(format=\"2006-01-02\") }}"
    );
}

#[test]
fn test_multiline_block_numeric_index_rewrite_applies() {
    assert_eq!(preprocess("{{\n  list.0 }}"), "{{\n  list[0] }}");
}

// ---- Roster coverage: registered builtins vs. positional handling ----

/// The positional-rewrite roster is hand-maintained, so a builtin registered
/// without a matching entry silently loses its Go form — the template that
/// uses it fails to parse in its entirety. Derive the check from the live
/// registration set (recorded at the engine-adapter boundary) so a new builtin
/// cannot join without a decision being recorded about its positional form.
#[test]
fn every_registered_builtin_has_positional_handling_or_an_exemption() {
    use super::positional::{NO_POSITIONAL_FORM, PREPROCESSED_ELSEWHERE, positional_builtin_names};
    use std::collections::BTreeSet;

    let registered = crate::template::registered_builtin_names();
    let accounted_for: BTreeSet<&str> = positional_builtin_names()
        .chain(PREPROCESSED_ELSEWHERE.iter().copied())
        .chain(NO_POSITIONAL_FORM.iter().copied())
        .collect();

    let unhandled: Vec<&str> = registered.difference(&accounted_for).copied().collect();
    assert!(
        unhandled.is_empty(),
        "registered builtin(s) with no Go positional form and no exemption: {unhandled:?}\n\
         Add each to POSITIONAL_FUNCTIONS/UNARY_FUNCTIONS in \
         template_preprocess/positional.rs, or — if it genuinely has no Go \
         call form — to NO_POSITIONAL_FORM with the reason."
    );

    let stale: Vec<&str> = accounted_for.difference(&registered).copied().collect();
    assert!(
        stale.is_empty(),
        "positional roster names(s) that are no longer registered builtins: {stale:?}"
    );
}

// ---- Positional form for the single-argument string builtins ----

#[test]
fn test_preprocess_positional_tolower_toupper() {
    assert_eq!(
        preprocess("{{ tolower \"ABC\" }}"),
        "{{ tolower(s=\"ABC\") }}"
    );
    assert_eq!(preprocess("{{ toupper .Os }}"), "{{ toupper(s=Os) }}");
}

#[test]
fn test_preprocess_positional_trim_and_title() {
    assert_eq!(
        preprocess("{{ trim \"  x  \" }}"),
        "{{ trim(s=\"  x  \") }}"
    );
    assert_eq!(
        preprocess("{{ title \"hello world\" }}"),
        "{{ title(s=\"hello world\") }}"
    );
}

#[test]
fn test_preprocess_positional_trimprefix_trimsuffix() {
    assert_eq!(
        preprocess("{{ trimprefix .Tag \"v\" }}"),
        "{{ trimprefix(s=Tag, prefix=\"v\") }}"
    );
    assert_eq!(
        preprocess("{{ trimsuffix .Name \".exe\" }}"),
        "{{ trimsuffix(s=Name, suffix=\".exe\") }}"
    );
}

#[test]
fn test_preprocess_piped_trimprefix_trimsuffix() {
    assert_eq!(
        preprocess("{{ .Tag | trimprefix \"v\" }}"),
        "{{ Tag | trimprefix(prefix=\"v\") }}"
    );
    assert_eq!(
        preprocess("{{ .Name | trimsuffix \".exe\" }}"),
        "{{ Name | trimsuffix(suffix=\".exe\") }}"
    );
}

/// A builtin with no argument-taking filter form must leave a pipe alone —
/// rewriting `| trim` to `| trim()` would shadow Tera's own zero-arg filter.
#[test]
fn test_zero_arg_pipes_are_left_alone() {
    for template in [
        "{{ Description | trim }}",
        "{{ Description | title }}",
        "{{ Version | incpatch }}",
        "{{ Path | readFile }}",
    ] {
        assert_eq!(preprocess(template), template);
    }
}

#[test]
fn test_preprocess_positional_path_and_digest_builtins() {
    assert_eq!(
        preprocess("{{ dir \"a/b/c.txt\" }}"),
        "{{ dir(s=\"a/b/c.txt\") }}"
    );
    assert_eq!(
        preprocess("{{ base .ArtifactPath }}"),
        "{{ base(s=ArtifactPath) }}"
    );
    assert_eq!(
        preprocess("{{ sha256 .ArtifactPath }}"),
        "{{ sha256(s=ArtifactPath) }}"
    );
    assert_eq!(
        preprocess("{{ sha3_512 .ArtifactPath }}"),
        "{{ sha3_512(s=ArtifactPath) }}"
    );
}

#[test]
fn test_preprocess_positional_version_and_env_builtins() {
    assert_eq!(
        preprocess("{{ incpatch .Version }}"),
        "{{ incpatch(v=Version) }}"
    );
    assert_eq!(
        preprocess("{{ isEnvSet \"CI\" }}"),
        "{{ isEnvSet(name=\"CI\") }}"
    );
    assert_eq!(
        preprocess("{{ envOrDefault \"CI\" \"no\" }}"),
        "{{ envOrDefault(name=\"CI\", default=\"no\") }}"
    );
    assert_eq!(
        preprocess("{{ indexOrDefault .Env \"CI\" \"no\" }}"),
        "{{ indexOrDefault(map=Env, key=\"CI\", default=\"no\") }}"
    );
}

/// The bare `list a b` call form collects into `items=[…]`; a lone `{{ list }}`
/// stays a variable reference.
#[test]
fn test_preprocess_bare_list_call() {
    assert_eq!(
        preprocess("{{ list \"a\" \"b\" }}"),
        "{{ list(items=[\"a\", \"b\"]) }}"
    );
    assert_eq!(preprocess("{{ list }}"), "{{ list }}");
    assert_eq!(preprocess("{{ list.0 }}"), "{{ list[0] }}");
}

/// Control blocks route through the same roster, so a newly rostered builtin
/// works inside `{% if %}` too.
#[test]
fn test_control_block_uses_extended_roster() {
    assert_eq!(
        preprocess("{{ if isEnvSet \"CI\" }}yes{{ end }}"),
        "{% if isEnvSet(name=\"CI\") %}yes{% endif %}"
    );
}

// ---- Sub-expression arguments ----

#[test]
fn test_subexpr_argument_in_standalone_call() {
    assert_eq!(
        preprocess("{{ trimprefix (base .Path) \"v\" }}"),
        "{{ trimprefix(s=(base(s=Path)), prefix=\"v\") }}"
    );
    assert_eq!(
        preprocess("{{ indexOrDefault (map \"k\" \"v\") \"k\" \"d\" }}"),
        "{{ indexOrDefault(map=(map(pairs=[\"k\", \"v\"])), key=\"k\", default=\"d\") }}"
    );
}

#[test]
fn test_subexpr_argument_nests() {
    assert_eq!(
        preprocess("{{ trimprefix (base (dir .Path)) \"v\" }}"),
        "{{ trimprefix(s=(base(s=(dir(s=Path)))), prefix=\"v\") }}"
    );
    assert_eq!(
        preprocess("{{ toupper (trimprefix (base (dir .Path)) \"v\") }}"),
        "{{ toupper(s=(trimprefix(s=(base(s=(dir(s=Path)))), prefix=\"v\"))) }}"
    );
}

/// The variadic builtins collect their trailing args into an array, so a
/// sub-expression in the final (unbounded) position has no arity to match
/// against — it must still be rewritten in place.
#[test]
fn test_subexpr_in_variadic_tail() {
    assert_eq!(
        preprocess("{{ printf \"%s-%s\" (tolower .Os) (base .Path) }}"),
        "{{ printf(format=\"%s-%s\", args=[(tolower(s=Os)), (base(s=Path))]) }}"
    );
    assert_eq!(
        preprocess("{{ print (tolower .Os) }}"),
        "{{ print(args=[(tolower(s=Os))]) }}"
    );
    assert_eq!(
        preprocess("{{ println (tolower .Os) }}"),
        "{{ println(args=[(tolower(s=Os))]) }}"
    );
    assert_eq!(
        preprocess("{{ list (tolower \"A\") (toupper \"b\") }}"),
        "{{ list(items=[(tolower(s=\"A\")), (toupper(s=\"b\"))]) }}"
    );
}

/// `slice` becomes a piped filter, so its item argument is the pipe input.
#[test]
fn test_subexpr_as_slice_item() {
    assert_eq!(
        preprocess("{{ slice (base .Path) 0 7 }}"),
        "{{ (base(s=Path)) | slice(start=0, end=7) }}"
    );
}

#[test]
fn test_subexpr_in_piped_call() {
    // After the pipe (the filter's own argument).
    assert_eq!(
        preprocess("{{ .Version | replace (base .Path) \"-\" }}"),
        "{{ Version | replace(from=(base(s=Path)), to=\"-\") }}"
    );
    // Before the pipe (the piped expression itself).
    assert_eq!(
        preprocess("{{ (base .Path) | trimprefix \"v\" }}"),
        "{{ (base(s=Path)) | trimprefix(prefix=\"v\") }}"
    );
    // Before the pipe, with no rewritable filter after it.
    assert_eq!(
        preprocess("{{ (base .Path) | upper }}"),
        "{{ (base(s=Path)) | upper }}"
    );
}

#[test]
fn test_subexpr_in_control_block() {
    assert_eq!(
        preprocess("{% if contains (tolower .Os) \"win\" %}W{% endif %}"),
        "{% if contains(s=(tolower(s=Os)), substr=\"win\") %}W{% endif %}"
    );
    // The Go statement form reaches the same rewrite through Pass 0.
    assert_eq!(
        preprocess("{{ if contains (tolower .Os) \"win\" }}W{{ end }}"),
        "{% if contains(s=(tolower(s=Os)), substr=\"win\") %}W{% endif %}"
    );
    // A sub-expression that is the whole condition.
    assert_eq!(
        preprocess("{% elif (isEnvSet \"CI\") %}C{% endif %}"),
        "{% elif (isEnvSet(name=\"CI\")) %}C{% endif %}"
    );
}

/// A sub-expression standing alone in a block has no enclosing call to trigger
/// an arity match, so the fallback pass is what rewrites it.
#[test]
fn test_subexpr_standing_alone() {
    assert_eq!(preprocess("{{ (tolower .Os) }}"), "{{ (tolower(s=Os)) }}");
    assert_eq!(
        preprocess("{{ (tolower .Os) ~ \"-\" ~ (base .Path) }}"),
        "{{ (tolower(s=Os)) ~ \"-\" ~ (base(s=Path)) }}"
    );
}

/// A parenthesis inside a string literal is string contents, never nesting.
#[test]
fn test_parens_inside_string_literals_do_not_nest() {
    assert_eq!(
        preprocess("{{ trimprefix (base \"x/(v9)\") \"(v\" }}"),
        "{{ trimprefix(s=(base(s=\"x/(v9)\")), prefix=\"(v\") }}"
    );
    // Under the raw string rule the backslash does not escape the quote, so
    // `"a\"` is a complete literal and the `)` that follows still closes the
    // group.
    assert_eq!(
        preprocess("{{ toupper (trimprefix \"a\\\" \"a\") }}"),
        "{{ toupper(s=(trimprefix(s=\"a\\\", prefix=\"a\"))) }}"
    );
}

/// Tera's own named-arg call syntax must stay untouched: a `(` glued to an
/// identifier opens a call, not a Go sub-expression.
#[test]
fn test_named_arg_calls_are_not_treated_as_subexpressions() {
    for template in [
        "{{ trimprefix(s=Path, prefix=\"v\") }}",
        "{{ Version | replace(from=\"v\", to=\"\") }}",
        "{{ trimprefix(s=base(s=Path), prefix=\"v\") }}",
        "{{ printf(format=\"%s\", args=[tolower(s=Os)]) }}",
    ] {
        assert_eq!(preprocess(template), template);
    }
}

/// Every rostered builtin accepts a sub-expression in its first positional
/// slot. Derived from the roster tables so a builtin added later is covered
/// without editing this test.
#[test]
fn test_every_rostered_builtin_accepts_a_subexpr_argument() {
    use super::positional::positional_specs;

    for (name, params) in positional_specs() {
        let filler = vec!["\"x\""; params.len() - 1];
        let mut call = format!("{{{{ {name} (tolower \"A\")");
        for arg in &filler {
            call.push(' ');
            call.push_str(arg);
        }
        call.push_str(" }}");

        let mut expected_params = vec![format!("{}=(tolower(s=\"A\"))", params[0])];
        for (param, arg) in params[1..].iter().zip(filler.iter()) {
            expected_params.push(format!("{param}={arg}"));
        }
        let expected = format!("{{{{ {name}({}) }}}}", expected_params.join(", "));

        assert_eq!(preprocess(&call), expected, "builtin `{name}`");
    }
}

// ---- Unbalanced expressions fail loudly ----

#[test]
fn test_unclosed_subexpr_is_reported_not_rewritten() {
    let input = "{{ trimprefix (base \"dist/v1\" \"v\" }}";
    // No rewrite may consume the rest of the block.
    assert_eq!(preprocess(input), input);

    let err = super::check_block_expressions(input)
        .expect_err("an unclosed group must be rejected")
        .to_string();
    assert!(err.contains("1 unclosed `(`"), "{err}");
    assert!(err.contains(input), "{err}");
    assert!(
        err.contains("trimprefix (base Path)"),
        "hint missing: {err}"
    );
}

#[test]
fn test_unmatched_close_paren_is_reported() {
    let err = super::check_block_expressions("{{ trimprefix base \"v\") }}")
        .expect_err("a stray `)` must be rejected")
        .to_string();
    assert!(err.contains("no matching `(`"), "{err}");
}

#[test]
fn test_multiple_unclosed_groups_are_counted() {
    let err = super::check_block_expressions("{{ toupper (trimprefix (base .Path \"v\" }}")
        .expect_err("two unclosed groups must be rejected")
        .to_string();
    assert!(err.contains("2 unclosed `(`"), "{err}");
}

/// An odd quote count swallows the closing paren, so the literal is named as
/// the cause rather than the paren count it produces.
#[test]
fn test_unterminated_string_literal_is_named_as_the_cause() {
    let err = super::check_block_expressions("{{ base (trimprefix \"a\\\"b/v1\" \"a\") }}")
        .expect_err("an unterminated literal must be rejected")
        .to_string();
    assert!(err.contains("unterminated string literal"), "{err}");
}

#[test]
fn test_balanced_expressions_pass_the_check() {
    for template in [
        "{{ Version }}",
        "{{ trimprefix (base .Path) \"v\" }}",
        "{{ trimprefix (base (dir .Path)) \"v\" }}",
        "{{ trimprefix (base \"x/(v9)\") \"(v\" }}",
        "{{ replace(s=Version, old=\"(\", new=\"\") }}",
        "{{ \"a b\" }}",
        "{% if contains (tolower .Os) \"win\" %}W{% endif %}",
    ] {
        assert!(
            super::check_block_expressions(template).is_ok(),
            "rejected a balanced template: {template}"
        );
    }
}

/// Raw-escaped text is emitted literally, so quoting broken syntax inside it
/// must keep rendering.
#[test]
fn test_raw_blocks_are_exempt_from_the_balance_check() {
    let raw = "{% raw %}{{ trimprefix (base \"x\" }}{% endraw %}";
    assert!(
        super::check_block_expressions(raw).is_ok(),
        "raw block rejected"
    );
    // The exemption ends at `endraw`.
    let after = "{% raw %}ok{% endraw %}{{ trimprefix (base \"x\" }}";
    assert!(super::check_block_expressions(after).is_err());
}

/// The diagnostic quotes the offending block, so a very long or multibyte
/// expression must neither flood the message nor split a codepoint.
#[test]
fn test_imbalance_diagnostic_is_bounded_and_utf8_safe() {
    let long = format!("{{{{ trimprefix (base \"{}\" }}}}", "日本語".repeat(80));
    let err = super::check_block_expressions(&long)
        .expect_err("still unbalanced")
        .to_string();
    assert!(std::str::from_utf8(err.as_bytes()).is_ok());
    assert!(err.contains('…'), "long block must be truncated: {err}");
}

// ---- Positional calls in `for` and `set` control blocks ----

#[test]
fn test_positional_call_in_for_block() {
    assert_eq!(
        preprocess("{% for x in filter .Lines \"^v\" %}{{ x }}{% endfor %}"),
        "{% for x in filter(items=Lines, regexp=\"^v\") %}{{ x }}{% endfor %}"
    );
    // Go's `range` reaches the same rewrite through Pass 0.
    assert_eq!(
        preprocess("{{ range filter .Lines \"^v\" }}{{ . }}{{ end }}"),
        "{% for val in filter(items=Lines, regexp=\"^v\") %}{{ val }}{% endfor %}"
    );
    // Sub-expression collection.
    assert_eq!(
        preprocess("{% for x in (filter .Lines \"^v\") %}{{ x }}{% endfor %}"),
        "{% for x in (filter(items=Lines, regexp=\"^v\")) %}{{ x }}{% endfor %}"
    );
    // The key/value form finds the same `in` separator.
    assert_eq!(
        preprocess("{% for k, v in filter .Lines \"^v\" %}{{ k }}{% endfor %}"),
        "{% for k, v in filter(items=Lines, regexp=\"^v\") %}{{ k }}{% endfor %}"
    );
}

#[test]
fn test_positional_call_in_set_block() {
    // Go's `{{ $v := trimprefix .Tag "v" }}`.
    assert_eq!(
        preprocess("{{ $v := trimprefix .Tag \"v\" }}"),
        "{% set v = trimprefix(s=Tag, prefix=\"v\") %}"
    );
    assert_eq!(
        preprocess("{{ $v := printf \"%s-%s\" .Os .Arch }}"),
        "{% set v = printf(format=\"%s-%s\", args=[Os, Arch]) %}"
    );
    assert_eq!(
        preprocess("{{ $v := trimprefix (base .Path) \"v\" }}"),
        "{% set v = trimprefix(s=(base(s=Path)), prefix=\"v\") %}"
    );
}

/// A plain collection or value must survive untouched — the rewrite only fires
/// when the expression actually is a Go call.
#[test]
fn test_plain_for_and_set_expressions_are_untouched() {
    for template in [
        "{% for x in Tags %}{{ x }}{% endfor %}",
        "{% for k, v in Env %}{{ k }}{% endfor %}",
        "{% for x in Tags | reverse %}{{ x }}{% endfor %}",
        "{% set v = Version %}",
        "{% set v = trimprefix(s=Tag, prefix=\"v\") %}",
        "{% endfor %}",
        "{% else %}",
    ] {
        assert_eq!(preprocess(template), template);
    }
}

// ---- Raw-escaped text is exempt from every pass ----

/// Text between `{% raw %}` and `{% endraw %}` reaches the engine literally, so
/// no pass may rewrite it. One shape per pass, since each pass owns a different
/// rewrite.
#[test]
fn test_raw_blocks_survive_every_pass_verbatim() {
    for shape in [
        "{{ if .X }}y{{ end }}",               // Pass 0 block conversion
        "{{ $v := .Tag }}",                    // Pass 0 assignment + `$` strip
        "{{ .Version }}",                      // Pass 1 leading dot
        "{{ in (list \"a\" \"b\") .Os }}",     // Pass 2 list sub-expression
        "{{ eq .A .B }}",                      // Pass 2b comparison
        "{{ len .Tags }}",                     // Pass 2b len
        "{{ map \"k\" \"v\" }}",               // Pass 2c map
        "{{ trimprefix .Tag \"v\" }}",         // Pass 3 standalone
        "{{ trimprefix (base .Path) \"v\" }}", // Pass 3 sub-expression
        "{{ .Version | replace \"v\" \"\" }}", // Pass 3 pipeline
        "{{ slice .Commit 0 7 }}",             // Pass 3 slice
        "{{ .Now.Format \"2006\" }}",          // Pass 4 method call
        "{{ list.0 }}",                        // Pass 5 numeric index
    ] {
        let template = format!("{{% raw %}}{shape}{{% endraw %}}");
        assert_eq!(preprocess(&template), template, "shape: {shape}");
    }
}

/// Every rostered builtin's positional call form is left alone inside raw.
/// Derived from the roster tables so a builtin added later is covered without
/// editing this test.
#[test]
fn test_raw_blocks_exempt_every_rostered_builtin() {
    use super::positional::positional_specs;

    for (name, params) in positional_specs() {
        let args = vec!["\"x\""; params.len()].join(" ");
        let template = format!("{{% raw %}}{{{{ {name} {args} }}}}{{% endraw %}}");
        assert_eq!(preprocess(&template), template, "builtin `{name}`");
    }
}

/// The exemption starts at `{% raw %}` and ends at `{% endraw %}` — a Go call
/// on either side is still rewritten.
#[test]
fn test_rewrites_resume_outside_the_raw_span() {
    assert_eq!(
        preprocess("{{ .A }}{% raw %}{{ .B }}{% endraw %}{{ .C }}"),
        "{{ A }}{% raw %}{{ .B }}{% endraw %}{{ C }}"
    );
    // Two spans with live text between them.
    assert_eq!(
        preprocess("{% raw %}{{ .A }}{% endraw %}{{ .B }}{% raw %}{{ .C }}{% endraw %}"),
        "{% raw %}{{ .A }}{% endraw %}{{ B }}{% raw %}{{ .C }}{% endraw %}"
    );
    // The whitespace-control spellings mark a span too.
    assert_eq!(
        preprocess("{%- raw -%}{{ .A }}{%- endraw -%}{{ .B }}"),
        "{%- raw -%}{{ .A }}{%- endraw -%}{{ B }}"
    );
}

/// An unterminated `{% raw %}` covers the rest of the template: the engine
/// rejects it, and the passes must not rewrite the text it marked literal on
/// the way to that error.
#[test]
fn test_unterminated_raw_covers_the_tail() {
    assert_eq!(
        preprocess("{{ .A }}{% raw %}{{ .B }}"),
        "{{ A }}{% raw %}{{ .B }}"
    );
}

/// Only a `{% … %}` tag delimits a span, matching tera's lexer — which scans
/// for the next `{%` and never inspects a `{{ … }}`. Reading `{{ endraw }}` as
/// a terminator ended the exemption early and rewrote the rest of the span;
/// reading `{{ raw }}` as an opener swallowed the rest of the template.
#[test]
fn test_expression_blocks_do_not_delimit_a_raw_span() {
    assert_eq!(
        preprocess("{% raw %}{{ endraw }}{{ .B }}{% endraw %}{{ .C }}"),
        "{% raw %}{{ endraw }}{{ .B }}{% endraw %}{{ C }}"
    );
    assert_eq!(preprocess("{{ raw }}{{ .B }}"), "{{ raw }}{{ B }}");
}

// ---- A positional call anywhere in a pipeline ----

/// Go accepts a positional call in every pipeline slot. Rewriting only the
/// segment after the last pipe left the earlier ones as raw Go syntax, and the
/// resulting parse error took the whole template with it.
#[test]
fn test_positional_call_before_a_pipe() {
    assert_eq!(
        preprocess("{{ trimprefix .Tag \"v\" | upper }}"),
        "{{ trimprefix(s=Tag, prefix=\"v\") | upper }}"
    );
    assert_eq!(
        preprocess("{{ .Version | replace \"v\" \"\" | upper }}"),
        "{{ Version | replace(from=\"v\", to=\"\") | upper }}"
    );
    assert_eq!(
        preprocess("{{ list \"a\" \"b\" | join(sep=\" \") }}"),
        "{{ list(items=[\"a\", \"b\"]) | join(sep=\" \") }}"
    );
    // A sub-expression argument inside a segment before the pipe.
    assert_eq!(
        preprocess("{{ printf \"%s\" (tolower .Os) | upper }}"),
        "{{ printf(format=\"%s\", args=[(tolower(s=Os))]) | upper }}"
    );
    // `slice`'s own rewrite introduces a pipe; a further filter chains onto it.
    assert_eq!(
        preprocess("{{ slice .Commit 0 7 | upper }}"),
        "{{ Commit | slice(start=0, end=7) | upper }}"
    );
    // The same rewrite reaches a control block's value expression.
    assert_eq!(
        preprocess("{% for x in filter .Lines \"^v\" | reverse %}{{ x }}{% endfor %}"),
        "{% for x in filter(items=Lines, regexp=\"^v\") | reverse %}{{ x }}{% endfor %}"
    );
}

/// Every segment of a chain is rewritten, not just the last one.
#[test]
fn test_every_pipeline_segment_is_rewritten() {
    assert_eq!(
        preprocess("{{ .Tag | trimprefix \"v\" | replace \".\" \"-\" | split \"-\" }}"),
        "{{ Tag | trimprefix(prefix=\"v\") | replace(from=\".\", to=\"-\") | split(sep=\"-\") }}"
    );
}

/// A `|` inside a string literal or a sub-expression is not a segment boundary:
/// the tokenizer captured each whole, so segmentation obeys exactly the literal
/// and paren rules the sub-expression rewrite obeys.
#[test]
fn test_pipe_inside_a_literal_or_subexpr_is_not_a_boundary() {
    assert_eq!(
        preprocess("{{ replace .Tag \"|\" \"-\" }}"),
        "{{ replace(s=Tag, old=\"|\", new=\"-\") }}"
    );
    assert_eq!(
        preprocess("{{ trimprefix (replace .Tag \"|\" \"-\") \"v\" }}"),
        "{{ trimprefix(s=(replace(s=Tag, old=\"|\", new=\"-\")), prefix=\"v\") }}"
    );
    assert_eq!(
        preprocess("{{ (replace .Tag \"a|b\" \"-\") | upper }}"),
        "{{ (replace(s=Tag, old=\"a|b\", new=\"-\")) | upper }}"
    );
}

// ---- Expression nesting is capped ----

/// `{{ tolower (tolower ( … "A" … )) }}` with `n` nested calls, so `n` is both
/// the call count and the parenthesis depth.
fn nested_calls(n: usize) -> String {
    format!("{{{{ {}\"A\"{} }}}}", "tolower (".repeat(n), ")".repeat(n))
}

/// Unbounded nesting used to kill the process — the rewriter rebuilds the whole
/// nest at every level — with no diagnostic at all.
#[test]
fn test_expression_nesting_is_capped_with_a_named_diagnostic() {
    let limit = super::MAX_EXPR_NESTING;
    super::check_block_expressions(&nested_calls(limit))
        .expect("the limit itself must be accepted");

    let err = super::check_block_expressions(&nested_calls(limit + 1))
        .expect_err("one level past the limit must be rejected")
        .to_string();
    assert!(err.contains("over-nested expression in template"), "{err}");
    assert!(
        err.contains(&format!(
            "parentheses nest {} deep, past the limit of {limit}",
            limit + 1
        )),
        "{err}"
    );
    // The offending block is quoted, bounded exactly as the imbalance
    // diagnostic bounds it.
    assert!(err.contains("{{ tolower (tolower"), "{err}");
    assert!(err.contains('…'), "long block must be truncated: {err}");
}

/// The rewriter's cap and the check's cap agree, so a template the check
/// accepts is rewritten all the way down with no Go call left behind.
#[test]
fn test_the_deepest_accepted_nesting_is_fully_rewritten() {
    let at_limit = nested_calls(super::MAX_EXPR_NESTING);
    super::check_block_expressions(&at_limit).expect("the limit must be accepted");
    let out = preprocess(&at_limit);
    assert!(!out.contains("tolower ("), "a Go call survived the rewrite");
    assert_eq!(out.matches("tolower(s=").count(), super::MAX_EXPR_NESTING);
}

/// The rewriter's own bound keeps the pass terminating for a caller that
/// skipped the check: past the cap a group is emitted verbatim instead of
/// descended into.
#[test]
fn test_preprocess_stops_descending_past_the_cap() {
    let past_limit = nested_calls(super::MAX_EXPR_NESTING + 20);
    let out = preprocess(&past_limit);
    // One rewrite per group entered (levels 1..=MAX), plus the block's own
    // outermost call, which sits at level 0 inside no group at all.
    assert_eq!(
        out.matches("tolower(s=").count(),
        super::MAX_EXPR_NESTING + 1
    );
    assert!(
        out.contains("tolower ("),
        "the tail past the cap must stay verbatim"
    );
}
