//! POSIX shell quoting.

/// Wrap `v` in POSIX single quotes so a shell reads it as exactly one literal
/// word.
///
/// Inside single quotes a POSIX shell performs no expansion at all, so `$`,
/// backticks, `"`, whitespace and newlines pass through untouched. The one
/// character that cannot appear is `'` itself; each is emitted as `'\''` —
/// close the quote, supply an escaped quote, reopen — which is why the result
/// is always quoted rather than quoted-if-needed.
///
/// ```
/// use anodizer_core::shell::shell_single_quote;
/// assert_eq!(shell_single_quote("it's"), r"'it'\''s'");
/// ```
pub fn shell_single_quote(v: &str) -> String {
    format!("'{}'", v.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_quote_escapes_embedded_quote() {
        assert_eq!(shell_single_quote("foo"), "'foo'");
        assert_eq!(shell_single_quote("foo's"), r"'foo'\''s'");
        assert_eq!(shell_single_quote(""), "''");
        assert_eq!(shell_single_quote(r#"a "b" c"#), r#"'a "b" c'"#);
        assert_eq!(shell_single_quote("$(rm -rf /)"), "'$(rm -rf /)'");
        assert_eq!(shell_single_quote("$HOME `id`"), "'$HOME `id`'");
        assert_eq!(shell_single_quote("a\nb"), "'a\nb'");
    }
}
