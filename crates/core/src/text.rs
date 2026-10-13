//! Plain-text shaping shared by stages: fitting a string into a byte budget
//! and spelling a count with the noun its number takes.

/// The marker anodizer appends where it cut an over-long string.
/// GoReleaser's is the same three-dot ellipsis.
pub const TRUNCATION_ELLIPSIS: &str = "...";

/// Cut `s` down to at most `max_len` bytes, ending it with
/// [`TRUNCATION_ELLIPSIS`], never splitting a UTF-8 character.
///
/// The budget is the whole result, marker included, because the callers'
/// limits (a GitHub release body, a drift summary line) are limits on what
/// they emit. A `max_len` too small to hold even the marker yields an empty
/// string — nothing of the original survives, and a partial marker would read
/// as content.
pub fn truncate_with_ellipsis(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    let Some(max_content) = max_len.checked_sub(TRUNCATION_ELLIPSIS.len()) else {
        return String::new();
    };
    let safe_end = s
        .char_indices()
        .map(|(i, c)| i + c.len_utf8())
        .take_while(|&end| end <= max_content)
        .last()
        .unwrap_or(0);
    format!("{}{}", &s[..safe_end], TRUNCATION_ELLIPSIS)
}

/// `n` followed by `singular` when `n` is 1, else by `plural`, so an
/// operator line reads `1 crate` and `2 crates`.
///
/// ```
/// use anodizer_core::text::count_noun;
/// assert_eq!(count_noun(1, "crate", "crates"), "1 crate");
/// assert_eq!(count_noun(0, "entry", "entries"), "0 entries");
/// ```
pub fn count_noun(n: usize, singular: &str, plural: &str) -> String {
    format!("{n} {}", if n == 1 { singular } else { plural })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_string_inside_the_budget_is_returned_whole() {
        assert_eq!(truncate_with_ellipsis("abcdef", 6), "abcdef");
    }

    #[test]
    fn the_marker_counts_against_the_budget() {
        assert_eq!(truncate_with_ellipsis("abcdef", 5), "ab...");
        assert_eq!(truncate_with_ellipsis("abcdef", 5).len(), 5);
    }

    #[test]
    fn a_multibyte_character_is_never_split() {
        // "é" is two bytes, so a 6-byte string under a 5-byte budget leaves
        // room for two content bytes: one whole character, never half of the
        // second.
        let out = truncate_with_ellipsis("ééé", 5);
        assert_eq!(out, "é...");
        assert!(out.len() <= 5);
    }

    #[test]
    fn a_budget_below_the_marker_yields_nothing() {
        assert_eq!(truncate_with_ellipsis("abcdef", 2), "");
    }

    #[test]
    fn count_noun_takes_the_singular_only_for_one() {
        assert_eq!(count_noun(0, "crate", "crates"), "0 crates");
        assert_eq!(count_noun(1, "crate", "crates"), "1 crate");
        assert_eq!(count_noun(2, "entry", "entries"), "2 entries");
    }

    /// Every string literal in `src`, raw strings included, with comments
    /// and char literals stepped over so a quote inside either is not read
    /// as the start of a string.
    fn string_literals(src: &str) -> Vec<String> {
        let b = src.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i..].starts_with(b"//") {
                i = src[i..].find('\n').map_or(b.len(), |n| i + n);
                continue;
            }
            if b[i..].starts_with(b"/*") {
                i = src[i + 2..].find("*/").map_or(b.len(), |n| i + n + 4);
                continue;
            }
            let ident_before = i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_');
            if b[i] == b'r' && !ident_before {
                let mut j = i + 1;
                while b.get(j) == Some(&b'#') {
                    j += 1;
                }
                if b.get(j) == Some(&b'"') {
                    let close = format!("\"{}", "#".repeat(j - i - 1));
                    let end = src[j + 1..].find(&close).map_or(b.len(), |n| j + 1 + n);
                    out.push(src[j + 1..end].to_string());
                    i = (end + close.len()).min(b.len());
                    continue;
                }
            }
            if b[i] == b'\'' {
                // A char literal ('"', '\'', '\u{..}'); a lifetime has no
                // closing quote and is stepped over as one character.
                let rest = &src[i + 1..];
                let len = if let Some(escaped) = rest.strip_prefix('\\') {
                    escaped.find('\'').map(|n| n + 2)
                } else {
                    rest.chars()
                        .next()
                        .filter(|c| rest[c.len_utf8()..].starts_with('\''))
                        .map(char::len_utf8)
                };
                i += len.map_or(1, |n| n + 2);
                continue;
            }
            if b[i] == b'"' {
                let mut j = i + 1;
                while j < b.len() && b[j] != b'"' {
                    j += if b[j] == b'\\' { 2 } else { 1 };
                }
                out.push(src[i + 1..j.min(b.len())].to_string());
                i = j + 1;
                continue;
            }
            i += 1;
        }
        out
    }

    #[test]
    fn string_literals_steps_over_comments_and_char_literals() {
        let src = "let q = '\"'; // \"not (s)\"\n/* \"x(s)\" */ f(\"a(s)\", r#\"b \"(es)\"#, 'a');";
        assert_eq!(string_literals(src), ["a(s)", "b \"(es)"]);
    }

    /// A count in an operator line is spelled for its number through
    /// [`count_noun`]. `item(s)` reads as a hedge, and every other line
    /// already says `1 crate` / `2 crates`, so one spelled the old way
    /// drifts from the rest. The allow-list names literals that are not a
    /// count, and each entry must still match a literal in the tree.
    #[test]
    fn no_production_string_spells_a_plural_in_parentheses() {
        use crate::test_helpers::test_sources::{production_half, workspace_production_sources};
        const NOT_A_COUNT: &[(&str, &str, &str)] = &[
            (
                "crates/stage-publish/src/schemastore/manifest.rs",
                "http(s)",
                "a URL scheme, http or https",
            ),
            (
                "crates/stage-publish/src/schemastore/preflight.rs",
                "http(s)",
                "a URL scheme, http or https",
            ),
            (
                "crates/stage-sign/src/process/mod.rs",
                "accepts 1 arg(s)",
                "cosign's own error text, matched to classify a failure",
            ),
            (
                "crates/stage-sign/src/process/mod.rs",
                "required flag(s)",
                "cosign's own error text, matched to classify a failure",
            ),
        ];
        let mut offenders = Vec::new();
        let mut allowed_seen = vec![false; NOT_A_COUNT.len()];
        let mut sources = 0;
        for source in workspace_production_sources() {
            let path = source.to_string_lossy().replace('\\', "/");
            let text = std::fs::read_to_string(&source).expect("read source");
            sources += 1;
            for literal in string_literals(production_half(&text)) {
                if !["(s)", "(es)", "(y|ies)", "(ies)"]
                    .iter()
                    .any(|form| literal.contains(form))
                {
                    continue;
                }
                let allowed = NOT_A_COUNT
                    .iter()
                    .position(|(file, needle, _)| path.ends_with(file) && literal.contains(needle));
                match allowed {
                    Some(index) => allowed_seen[index] = true,
                    None => offenders.push(format!("{path}: {literal:?}")),
                }
            }
        }
        assert!(sources > 500, "the walk found only {sources} sources");
        assert!(
            offenders.is_empty(),
            "spell these counts with anodizer_core::text::count_noun, or write the \
             plural where no count is printed: {offenders:#?}"
        );
        let stale: Vec<_> = NOT_A_COUNT
            .iter()
            .zip(&allowed_seen)
            .filter(|(_, seen)| !**seen)
            .map(|(entry, _)| entry)
            .collect();
        assert!(
            stale.is_empty(),
            "allow-list entries no longer in the tree: {stale:#?}"
        );
    }
}
