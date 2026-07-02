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
fn test_preprocess_list_subexpr_escaped_double_quotes() {
    // (list "hello \"world\"" "plain") should parse correctly
    let input = r#"{{ in (list "hello \"world\"" "plain") "plain" }}"#;
    let result = preprocess(input);
    assert_eq!(
        result,
        r#"{{ in(items=["hello \"world\"", "plain"], value="plain") }}"#
    );
}

#[test]
fn test_preprocess_list_subexpr_escaped_single_quotes() {
    // (list 'it\'s' 'fine') should parse correctly
    let input = "{{ in (list 'it\\'s' 'fine') \"fine\" }}";
    let result = preprocess(input);
    assert_eq!(result, "{{ in(items=['it\\'s', 'fine'], value=\"fine\") }}");
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
    // Note: `index m "a"` inside parens is NOT rewritten by Pass 3
    // (positional rewriter only handles top-level standalone/piped forms).
    assert_eq!(
        result,
        "{% set m = map(pairs=[\"a\", \"1\"]) %}{% if (index m \"a\") == \"1\" %}yes{% endif %}"
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
