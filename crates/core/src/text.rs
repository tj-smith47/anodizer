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

    /// Every string literal in `src` (raw and byte strings included) and
    /// the text of every doc comment (`///`, `//!`, `/** */`, `/*! */`),
    /// with plain comments and char literals stepped over so a quote inside
    /// either is not read as the start of a string.
    fn literals_and_docs(src: &str) -> (Vec<String>, Vec<String>) {
        let b = src.as_bytes();
        let mut literals = Vec::new();
        let mut docs = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i..].starts_with(b"//") {
                let end = src[i..].find('\n').map_or(b.len(), |n| i + n);
                let comment = &src[i..end];
                if (comment.starts_with("///") && !comment.starts_with("////"))
                    || comment.starts_with("//!")
                {
                    docs.push(comment[3..].to_string());
                }
                i = end;
                continue;
            }
            if b[i..].starts_with(b"/*") {
                // Block comments nest, so the comment ends where the depth
                // returns to zero.
                let mut depth = 0usize;
                let mut j = i;
                while j < b.len() {
                    if b[j..].starts_with(b"/*") {
                        depth += 1;
                        j += 2;
                    } else if b[j..].starts_with(b"*/") {
                        depth -= 1;
                        j += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        j += 1;
                    }
                }
                let comment = &src[i..j];
                let is_doc = (comment.starts_with("/**")
                    && !comment.starts_with("/***")
                    && comment != "/**/")
                    || comment.starts_with("/*!");
                if is_doc {
                    docs.push(comment[3..comment.len().saturating_sub(2).max(3)].to_string());
                }
                i = j;
                continue;
            }
            let ident_before = i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_');
            let raw_at = match b[i] {
                b'r' if !ident_before => Some(i + 1),
                b'b' | b'c' if !ident_before && b.get(i + 1) == Some(&b'r') => Some(i + 2),
                _ => None,
            };
            if let Some(mut j) = raw_at {
                let hashes_from = j;
                while b.get(j) == Some(&b'#') {
                    j += 1;
                }
                if b.get(j) == Some(&b'"') {
                    let close = format!("\"{}", "#".repeat(j - hashes_from));
                    let end = src[j + 1..].find(&close).map_or(b.len(), |n| j + 1 + n);
                    literals.push(src[j + 1..end].to_string());
                    i = (end + close.len()).min(b.len());
                    continue;
                }
            }
            if b[i] == b'\'' {
                // A char literal ('"', '\'', '\\', '\n', '\u{..}'); a
                // lifetime has no closing quote and is stepped over as one
                // character.
                let rest = &src[i + 1..];
                let len = if let Some(escaped) = rest.strip_prefix('\\') {
                    let body = if escaped.starts_with("u{") {
                        escaped.find('}').map(|n| n + 1)
                    } else {
                        escaped.chars().next().map(char::len_utf8)
                    };
                    body.filter(|&n| escaped[n..].starts_with('\''))
                        .map(|n| n + 1)
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
                literals.push(src[i + 1..j.min(b.len())].to_string());
                i = j + 1;
                continue;
            }
            i += 1;
        }
        (literals, docs)
    }

    #[test]
    fn string_literals_steps_over_comments_and_char_literals() {
        let src = "let q = '\"'; // \"not (s)\"\n/* \"x(s)\" */ f(\"a(s)\", r#\"b \"(es)\"#, 'a');";
        assert_eq!(literals_and_docs(src).0, ["a(s)", "b \"(es)"]);
    }

    /// An escaped char literal is stepped over whole, so the string right
    /// after it still opens where it does.
    #[test]
    fn escaped_char_literals_step_over_their_escape() {
        let src = r#"let a = '\n'"a(s)"; let b = '\\'"b(s)"; let c = '\u{1F600}'"c(s)"; let d = '\''"d(s)";"#;
        assert_eq!(literals_and_docs(src).0, ["a(s)", "b(s)", "c(s)", "d(s)"]);
    }

    /// `br#"…"#` is a raw string and `b"…"` a string.
    #[test]
    fn byte_strings_are_read_as_strings() {
        let src = r###"let d = br#"d "(s)"#; let e = b"e(es)"; let f = cr##"f "#(s)"##;"###;
        assert_eq!(literals_and_docs(src).0, ["d \"(s)", "e(es)", "f \"#(s)"]);
    }

    /// A block comment nested inside another ends where the outer one does.
    #[test]
    fn nested_block_comments_are_stepped_over_whole() {
        let src = "/* outer /* inner */ \"not(s)\" */ let f = \"f(s)\";";
        assert_eq!(literals_and_docs(src).0, ["f(s)"]);
    }

    /// The doc comments are collected; a plain comment and a `////` line
    /// are not.
    #[test]
    fn doc_comments_are_collected() {
        let src = "/// item (s)\n//! module (es)\n//// rule\n// plain (s)\n/** block (s) */\n/*! inner */\nfn x() {}";
        assert_eq!(
            literals_and_docs(src).1,
            [" item (s)", " module (es)", " block (s) ", " inner "]
        );
    }

    /// A count in an operator line is spelled for its number through
    /// [`count_noun`]. `item(s)` reads as a hedge, and every other line
    /// already says `1 crate` / `2 crates`, so one spelled the old way
    /// drifts from the rest. Doc comments are read too: clap takes a
    /// command's help from them, and the configuration reference and
    /// schema are generated from them. The allow-list names text that is
    /// not a count, and each entry must still match text in the tree.
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
            (
                "crates/stage-sign/src/process/mod.rs",
                "accepts at most 1 arg(s)",
                "cosign's own error text, quoted in the doc of its classifier",
            ),
            (
                "crates/core/src/git/remote.rs",
                "http(s)://",
                "a URL scheme, http or https",
            ),
            (
                "crates/stage-appbundle/src/lib.rs",
                "`Glob(s)`",
                "an enum variant binding `s`",
            ),
            (
                "crates/stage-blob/src/preflight.rs",
                "`Ok(Some(Some(s)))`",
                "a pattern binding `s`",
            ),
            (
                "crates/stage-nfpm/src/run/mod.rs",
                "Some(ref s)",
                "a pattern binding `s`",
            ),
        ];
        let mut offenders = Vec::new();
        let mut allowed_seen = vec![false; NOT_A_COUNT.len()];
        let mut sources = 0;
        for source in workspace_production_sources() {
            let path = source.to_string_lossy().replace('\\', "/");
            let text = std::fs::read_to_string(&source).expect("read source");
            sources += 1;
            let (literals, docs) = literals_and_docs(production_half(&text));
            for literal in literals.into_iter().chain(docs) {
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
