//! Shell-style `$VAR` / `${VAR}` expansion with a pluggable lookup.
//!
//! Matches GoReleaser's `os.ExpandEnv()` shell rules:
//!   - Variable names start with `_` or ASCII letter.
//!   - `$5` and similar digit-prefixed sequences are NOT expanded (kept literal).
//!   - `${...}` without a closing `}` is kept literal (`${` + consumed text).
//!   - Unknown variables expand to empty string (no error).
//!
//! Used by:
//!   - `cli::pipeline` for processing config values against process env.
//!   - `stage-build` for build-env expansion against process env.
//!   - `stage-sign` for signing-arg substitution against a fixed map.

/// Expand `$VAR` and `${VAR}` references in `s`, looking up values via `lookup`.
///
/// - `lookup` returns `Some(value)` for a known variable, `None` to expand to `""`
///   (GoReleaser behavior for unset vars).
/// - Expansion is single-pass: a looked-up value is NOT re-scanned for further `$`.
pub fn expand_with<F>(s: &str, mut lookup: F) -> String
where
    F: FnMut(&str) -> Option<String>,
{
    let mut result = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        if chars[i] != '$' {
            result.push(chars[i]);
            i += 1;
            continue;
        }

        // `$` at end of string — keep literal.
        if i + 1 >= len {
            result.push('$');
            i += 1;
            continue;
        }

        // `${VAR}` form.
        if chars[i + 1] == '{' {
            if let Some(close) = chars[i + 2..].iter().position(|&c| c == '}') {
                let var_name: String = chars[i + 2..i + 2 + close].iter().collect();
                if let Some(val) = lookup(&var_name) {
                    result.push_str(&val);
                }
                i += 2 + close + 1;
            } else {
                // No closing brace — keep `${` + consumed text literal.
                result.push('$');
                i += 1;
            }
            continue;
        }

        // `$VAR` form: names start with `_` or ASCII letter (shell rules).
        // Digits (`$5`) are kept literal to match GoReleaser.
        let starts_valid = chars[i + 1].is_ascii_alphabetic() || chars[i + 1] == '_';
        if !starts_valid {
            result.push('$');
            i += 1;
            continue;
        }

        let start = i + 1;
        let mut end = start;
        while end < len && (chars[end].is_ascii_alphanumeric() || chars[end] == '_') {
            end += 1;
        }
        let var_name: String = chars[start..end].iter().collect();
        if let Some(val) = lookup(&var_name) {
            result.push_str(&val);
        }
        i = end;
    }

    result
}

/// Convenience wrapper that looks up variables in the process environment.
pub fn expand_env(s: &str) -> String {
    expand_with(s, |name| std::env::var(name).ok())
}

/// Like `expand_with`, but preserves the `$VAR` / `${VAR}` literal when
/// `lookup` returns `None` (instead of expanding to empty string).
///
/// Used by signing-arg substitution where the lookup map is small and
/// closed: unmatched `$names` must pass through unchanged so paths like
/// `/tmp/$HOME/file` survive rendering without being eaten.
pub fn expand_with_preserve<F>(s: &str, mut lookup: F) -> String
where
    F: FnMut(&str) -> Option<String>,
{
    let mut result = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        if chars[i] != '$' {
            result.push(chars[i]);
            i += 1;
            continue;
        }
        if i + 1 >= len {
            result.push('$');
            i += 1;
            continue;
        }
        if chars[i + 1] == '{' {
            if let Some(close) = chars[i + 2..].iter().position(|&c| c == '}') {
                let var_name: String = chars[i + 2..i + 2 + close].iter().collect();
                match lookup(&var_name) {
                    Some(val) => result.push_str(&val),
                    None => {
                        // Preserve literal `${name}`.
                        result.push('$');
                        result.push('{');
                        result.push_str(&var_name);
                        result.push('}');
                    }
                }
                i += 2 + close + 1;
            } else {
                result.push('$');
                i += 1;
            }
            continue;
        }
        let starts_valid = chars[i + 1].is_ascii_alphabetic() || chars[i + 1] == '_';
        if !starts_valid {
            result.push('$');
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut end = start;
        while end < len && (chars[end].is_ascii_alphanumeric() || chars[end] == '_') {
            end += 1;
        }
        let var_name: String = chars[start..end].iter().collect();
        match lookup(&var_name) {
            Some(val) => result.push_str(&val),
            None => {
                // Preserve literal `$name`.
                result.push('$');
                result.push_str(&var_name);
            }
        }
        i = end;
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup_map<'a>(m: &'a [(&'a str, &'a str)]) -> impl FnMut(&str) -> Option<String> + 'a {
        move |name: &str| {
            m.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn braced_form_expands() {
        let vars = [("HOME", "/home/tj"), ("USER", "tj")];
        assert_eq!(expand_with("${HOME}", lookup_map(&vars)), "/home/tj");
        assert_eq!(expand_with("a${USER}b", lookup_map(&vars)), "atjb");
    }

    #[test]
    fn bare_form_expands() {
        let vars = [("FOO", "bar")];
        assert_eq!(expand_with("$FOO", lookup_map(&vars)), "bar");
        assert_eq!(expand_with("pre_$FOO", lookup_map(&vars)), "pre_bar");
    }

    #[test]
    fn digit_sequences_not_expanded() {
        // `$5` must NOT be expanded — matches GoReleaser + shell rules.
        // This is the D1 bug fix case.
        assert_eq!(expand_with("Bearer $5XYZ", |_| None), "Bearer $5XYZ");
        assert_eq!(expand_with("$0", |_| None), "$0");
    }

    #[test]
    fn unset_var_expands_to_empty() {
        assert_eq!(expand_with("a${UNSET}b", |_| None), "ab");
        assert_eq!(expand_with("a$UNSET b", |_| None), "a b");
    }

    #[test]
    fn unclosed_brace_kept_literal() {
        assert_eq!(expand_with("${FOO", |_| None), "${FOO");
    }

    #[test]
    fn dollar_at_end_kept_literal() {
        assert_eq!(expand_with("end$", |_| None), "end$");
    }

    #[test]
    fn dollar_before_non_var_kept_literal() {
        assert_eq!(expand_with("$ space", |_| None), "$ space");
        assert_eq!(expand_with("$.field", |_| None), "$.field");
    }

    #[test]
    fn single_pass_no_recursion() {
        // If $A expands to "$B", the $B must NOT be re-expanded.
        let vars = [("A", "$B"), ("B", "expanded")];
        assert_eq!(expand_with("$A", lookup_map(&vars)), "$B");
    }

    #[test]
    fn underscore_start_valid() {
        let vars = [("_HIDDEN", "x")];
        assert_eq!(expand_with("$_HIDDEN", lookup_map(&vars)), "x");
    }

    #[test]
    fn longest_match_greedy() {
        let vars = [("FOO", "short"), ("FOOBAR", "long")];
        assert_eq!(expand_with("$FOOBAR", lookup_map(&vars)), "long");
        assert_eq!(expand_with("$FOO", lookup_map(&vars)), "short");
    }

    #[test]
    fn empty_string() {
        assert_eq!(expand_with("", |_| None), "");
    }

    #[test]
    fn no_dollar_signs() {
        assert_eq!(expand_with("plain text", |_| None), "plain text");
    }
}
