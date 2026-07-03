//! Single source of truth for tera 1.x raw string-literal boundary rules.
//!
//! The engine closes a string literal at the FIRST next occurrence of its
//! opening delimiter — `"`, `'`, or backtick — with no escape concept: a
//! backslash never protects the character after it. tera 2.0's escape-aware
//! lexer is held to that same boundary by the engine-side compat shim
//! (`template::engine_adapter::double_string_literal_backslashes`), which
//! doubles backslashes so both engines agree on the close position by
//! construction — and which locates each literal via [`is_string_delim`] /
//! [`raw_string_end`] below, so the shim cannot drift from the passes. Every
//! preprocessor pass that skips string literals must go through these
//! helpers: a pass with its own (escape-aware, or two-delimiter) scanner
//! disagrees with the engine about where a string ends and silently rewrites
//! string contents — or rewrites code it wrongly thought was quoted.

/// True when `b` is a tera string-literal delimiter (`"`, `'`, or backtick).
pub(crate) fn is_string_delim(b: u8) -> bool {
    matches!(b, b'"' | b'\'' | b'`')
}

/// Given that `bytes[start]` is a string delimiter, return the index just
/// past the literal's closing delimiter under the raw rule: first next
/// occurrence of the same delimiter, no escape awareness. Returns
/// `bytes.len()` for an unterminated literal so callers' scan loops
/// terminate cleanly.
pub(crate) fn raw_string_end(bytes: &[u8], start: usize) -> usize {
    let delim = bytes[start];
    match bytes[start + 1..].iter().position(|&b| b == delim) {
        Some(off) => start + 1 + off + 1,
        None => bytes.len(),
    }
}

/// Copy the string literal opening at `s[i]` into `out` verbatim and return
/// the index just past its closing delimiter (see [`raw_string_end`]).
pub(super) fn copy_raw_string(out: &mut String, s: &str, i: usize) -> usize {
    // Delimiters are ASCII, so the end index is always a char boundary.
    let end = raw_string_end(s.as_bytes(), i);
    out.push_str(&s[i..end]);
    end
}

/// Regex alternation matching one complete raw string literal, for the
/// regex-based passes. Semantically identical to [`raw_string_end`]:
/// three delimiters, first-occurrence close, no escape awareness. Embed as
/// `(?:{RAW_STRING_RE_ALT})` when combined with other alternatives.
pub(super) const RAW_STRING_RE_ALT: &str = r#""[^"]*"|'[^']*'|`[^`]*`"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closes_at_first_delimiter_ignoring_backslash() {
        // `'Q\'` — the quote right after the backslash IS the close.
        let s = r"'Q\' ~ 'v1.0'";
        assert_eq!(raw_string_end(s.as_bytes(), 0), 4);
    }

    #[test]
    fn backtick_is_a_delimiter() {
        let s = "`v1.0` tail";
        assert_eq!(raw_string_end(s.as_bytes(), 0), 6);
    }

    #[test]
    fn unterminated_returns_len() {
        let s = r#""never closed"#;
        assert_eq!(raw_string_end(s.as_bytes(), 0), s.len());
    }

    #[test]
    fn copy_raw_string_is_verbatim() {
        let mut out = String::new();
        let s = r"'a\n日本' rest";
        let end = copy_raw_string(&mut out, s, 0);
        assert_eq!(out, r"'a\n日本'");
        assert_eq!(&s[end..], " rest");
    }

    #[test]
    fn regex_fragment_agrees_with_scanner_on_boundaries() {
        // The regex alternation and the byte scanner implement the same rule;
        // this pins them together so neither can drift alone.
        let re = regex::Regex::new(RAW_STRING_RE_ALT).expect("valid fragment");
        let samples = [
            r#""plain" x"#,
            r"'Q\' tail",
            "`v1.0` tail",
            r#""a\"b" tail"#,
            r"'it\'s'",
        ];
        for s in samples {
            let m = re.find(s).expect("fragment must match a leading literal");
            assert_eq!(m.start(), 0, "sample: {s}");
            assert_eq!(
                m.end(),
                raw_string_end(s.as_bytes(), 0),
                "boundary drift for sample: {s}"
            );
        }
    }
}
