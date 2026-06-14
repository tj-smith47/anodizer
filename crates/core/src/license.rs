//! SPDX license-expression parsing shared across package-manifest builders.
//!
//! Homebrew, Nix, AUR, and winget each render a project's license differently:
//! Homebrew wants `license any_of: ["A", "B"]` for an `A OR B` expression and
//! `license all_of: [...]` for `A AND B`; Nix wants a `with lib.licenses; [ a b
//! ]` list; winget wants a single short identifier. They all need the same
//! upstream fact first — *which SPDX ids does this expression name, and how are
//! they joined?* — so that fact is parsed once here and each builder renders its
//! own syntax from the structured result.
//!
//! The parser is intentionally forgiving: a license string is project metadata,
//! not a security boundary, and a manifest builder must never panic or abort a
//! release because a `Cargo.toml` carries an SPDX form this parser does not
//! model. Anything it cannot decompose degrades to a single
//! [`SpdxExpr::Single`] literal carrying the original text verbatim, so the
//! worst case is "render the string as-is" — exactly today's behavior.

/// A parsed SPDX license expression.
///
/// Only the two connectives Homebrew/Nix care about (`OR` / `AND`) are modeled
/// as compound variants; everything else (a bare id, a `WITH` exception, an
/// unparseable compound) collapses into [`SpdxExpr::Single`] so callers always
/// have a literal to fall back on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpdxExpr {
    /// A single license identifier or any expression this parser chose to keep
    /// literal (e.g. `MIT`, `Apache-2.0 WITH LLVM-exception`, or a parenthesised
    /// compound it could not flatten). The string is the verbatim source text
    /// with only surrounding whitespace/parentheses trimmed.
    Single(String),
    /// A disjunction: `A OR B [OR C ...]`. Renders as Homebrew `any_of:`.
    AnyOf(Vec<String>),
    /// A conjunction: `A AND B [AND C ...]`. Renders as Homebrew `all_of:`.
    AllOf(Vec<String>),
}

impl SpdxExpr {
    /// The distinct license ids this expression names, in source order.
    ///
    /// For [`SpdxExpr::Single`] this is a one-element slice carrying the literal.
    pub fn ids(&self) -> &[String] {
        match self {
            // A single-element view backed by the inner String. `std::slice`
            // from a reference keeps the borrow tied to `self`.
            Self::Single(s) => std::slice::from_ref(s),
            Self::AnyOf(v) | Self::AllOf(v) => v,
        }
    }

    /// True when the expression is a single literal (no `OR`/`AND` connective).
    pub fn is_single(&self) -> bool {
        matches!(self, Self::Single(_))
    }
}

/// Parse an SPDX license expression into its ids + connective.
///
/// Handles the forms that appear in real `Cargo.toml` / crate metadata:
/// - a single id — `MIT` → [`SpdxExpr::Single`]
/// - disjunction — `Apache-2.0 OR MIT` → [`SpdxExpr::AnyOf`]
/// - conjunction — `Apache-2.0 AND MIT` → [`SpdxExpr::AllOf`]
/// - a `WITH` exception — `Apache-2.0 WITH LLVM-exception` → kept literal as
///   [`SpdxExpr::Single`] (the exception is part of one license, not a list)
/// - the legacy slash form — `MIT/Apache-2.0` → [`SpdxExpr::AnyOf`] (older
///   crates used `/` as the `OR` separator before SPDX 2.1)
///
/// Mixed-connective (`A AND B OR C`) and parenthesised/nested forms degrade to a
/// single literal rather than guessing precedence: a builder rendering the
/// verbatim string is always valid Ruby/Nix, whereas a mis-flattened list could
/// silently drop or reorder a clause. Never panics; never returns an empty list.
pub fn parse_spdx_expression(expr: &str) -> SpdxExpr {
    let trimmed = expr.trim();
    if trimmed.is_empty() {
        return SpdxExpr::Single(String::new());
    }

    // Parentheses imply grouping/precedence this flat splitter can't honor
    // safely — keep the whole thing literal (trimming only an outer wrapping
    // pair so `(MIT)` still reads as the bare id).
    let unwrapped = strip_outer_parens(trimmed);
    if unwrapped.contains('(') || unwrapped.contains(')') {
        return SpdxExpr::Single(unwrapped.to_string());
    }

    // A `WITH` exception binds tighter than OR/AND and names a single license;
    // treat the whole expression as one literal unless it also carries a
    // top-level connective (which would make precedence ambiguous → literal too).
    let has_or = has_top_level_keyword(unwrapped, "OR") || unwrapped.contains('/');
    let has_and = has_top_level_keyword(unwrapped, "AND");
    let has_with = has_top_level_keyword(unwrapped, "WITH");

    // Mixed or exception-bearing compounds: don't guess — render verbatim.
    if (has_or && has_and) || has_with {
        return SpdxExpr::Single(unwrapped.to_string());
    }

    if has_or {
        let ids = split_keyword(unwrapped, "OR");
        if ids.len() >= 2 {
            return SpdxExpr::AnyOf(ids);
        }
    }
    if has_and {
        let ids = split_keyword(unwrapped, "AND");
        if ids.len() >= 2 {
            return SpdxExpr::AllOf(ids);
        }
    }

    SpdxExpr::Single(unwrapped.to_string())
}

/// Strip one matching outer `( ... )` pair (and surrounding whitespace) when it
/// wraps the *entire* expression. Leaves inner parens untouched.
fn strip_outer_parens(s: &str) -> &str {
    let t = s.trim();
    if !t.starts_with('(') || !t.ends_with(')') {
        return t;
    }
    // Confirm the opening paren matches the closing one (not `(A) OR (B)`,
    // whose first `(` closes mid-string).
    let mut depth = 0u32;
    for (i, ch) in t.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                // The outer `(` closes before the final char → not a full wrap.
                if depth == 0 && i != t.len() - 1 {
                    return t;
                }
            }
            _ => {}
        }
    }
    strip_outer_parens(t[1..t.len() - 1].trim())
}

/// True when `kw` (a whole-word SPDX keyword like `OR`/`AND`/`WITH`) appears as
/// a top-level token in `expr`. Case-sensitive per SPDX (operators are
/// uppercase); matched on whitespace boundaries so it never fires inside an id
/// such as `LiLiQ-R` or `Apache-2.0`.
fn has_top_level_keyword(expr: &str, kw: &str) -> bool {
    expr.split_whitespace().any(|tok| tok == kw)
}

/// Split on the connective keyword (`OR`/`AND`), also honoring the legacy `/`
/// separator for `OR`. Returns trimmed, non-empty operands in source order.
fn split_keyword(expr: &str, kw: &str) -> Vec<String> {
    // Normalize the legacy slash form to the keyword so a single splitter
    // handles `MIT/Apache-2.0` and `MIT OR Apache-2.0` identically.
    let normalized = if kw == "OR" {
        expr.replace('/', " OR ")
    } else {
        expr.to_string()
    };
    normalized
        .split_whitespace()
        .collect::<Vec<_>>()
        .split(|tok| *tok == kw)
        .map(|chunk| chunk.join(" "))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_id() {
        assert_eq!(
            parse_spdx_expression("MIT"),
            SpdxExpr::Single("MIT".to_string())
        );
        assert!(parse_spdx_expression("MIT").is_single());
        assert_eq!(parse_spdx_expression("MIT").ids(), &["MIT".to_string()]);
    }

    #[test]
    fn single_id_trims_whitespace() {
        assert_eq!(
            parse_spdx_expression("  Apache-2.0  "),
            SpdxExpr::Single("Apache-2.0".to_string())
        );
    }

    #[test]
    fn empty_is_empty_single() {
        assert_eq!(parse_spdx_expression(""), SpdxExpr::Single(String::new()));
        assert_eq!(
            parse_spdx_expression("   "),
            SpdxExpr::Single(String::new())
        );
    }

    #[test]
    fn or_two() {
        assert_eq!(
            parse_spdx_expression("Apache-2.0 OR MIT"),
            SpdxExpr::AnyOf(vec!["Apache-2.0".to_string(), "MIT".to_string()])
        );
    }

    #[test]
    fn or_three() {
        assert_eq!(
            parse_spdx_expression("MIT OR Apache-2.0 OR BSD-3-Clause"),
            SpdxExpr::AnyOf(vec![
                "MIT".to_string(),
                "Apache-2.0".to_string(),
                "BSD-3-Clause".to_string(),
            ])
        );
    }

    #[test]
    fn and_two() {
        assert_eq!(
            parse_spdx_expression("Apache-2.0 AND MIT"),
            SpdxExpr::AllOf(vec!["Apache-2.0".to_string(), "MIT".to_string()])
        );
    }

    #[test]
    fn legacy_slash_is_or() {
        assert_eq!(
            parse_spdx_expression("MIT/Apache-2.0"),
            SpdxExpr::AnyOf(vec!["MIT".to_string(), "Apache-2.0".to_string()])
        );
    }

    #[test]
    fn legacy_slash_with_spaces() {
        assert_eq!(
            parse_spdx_expression("MIT / Apache-2.0"),
            SpdxExpr::AnyOf(vec!["MIT".to_string(), "Apache-2.0".to_string()])
        );
    }

    #[test]
    fn with_exception_stays_literal() {
        assert_eq!(
            parse_spdx_expression("Apache-2.0 WITH LLVM-exception"),
            SpdxExpr::Single("Apache-2.0 WITH LLVM-exception".to_string())
        );
    }

    #[test]
    fn mixed_connectives_stay_literal() {
        // No precedence guessing: `A AND B OR C` is ambiguous to a flat
        // splitter, so it degrades to a verbatim literal.
        assert_eq!(
            parse_spdx_expression("Apache-2.0 AND MIT OR BSD-3-Clause"),
            SpdxExpr::Single("Apache-2.0 AND MIT OR BSD-3-Clause".to_string())
        );
    }

    #[test]
    fn parenthesised_compound_stays_literal() {
        assert_eq!(
            parse_spdx_expression("(MIT OR Apache-2.0) AND BSD-3-Clause"),
            SpdxExpr::Single("(MIT OR Apache-2.0) AND BSD-3-Clause".to_string())
        );
    }

    #[test]
    fn outer_parens_stripped_for_simple_or() {
        // A fully-wrapping outer pair is removed before splitting.
        assert_eq!(
            parse_spdx_expression("(MIT OR Apache-2.0)"),
            SpdxExpr::AnyOf(vec!["MIT".to_string(), "Apache-2.0".to_string()])
        );
    }

    #[test]
    fn outer_parens_stripped_for_bare_id() {
        assert_eq!(
            parse_spdx_expression("(MIT)"),
            SpdxExpr::Single("MIT".to_string())
        );
    }

    #[test]
    fn non_wrapping_parens_kept_literal() {
        // `(A) OR (B)` — the first `(` closes mid-string, so it is NOT a full
        // wrap; the inner parens force a literal.
        assert_eq!(
            parse_spdx_expression("(MIT) OR (Apache-2.0)"),
            SpdxExpr::Single("(MIT) OR (Apache-2.0)".to_string())
        );
    }

    #[test]
    fn or_substring_in_id_not_split() {
        // `OR` must match as a whole token, never inside an id. There is no
        // real SPDX id containing a bare ` OR ` token, so a single id with
        // letters is returned untouched.
        assert_eq!(
            parse_spdx_expression("CERN-OHL-S-2.0"),
            SpdxExpr::Single("CERN-OHL-S-2.0".to_string())
        );
    }

    #[test]
    fn lowercase_or_is_not_a_connective() {
        // SPDX operators are uppercase; a lowercase `or` is treated as part of
        // a (malformed) literal, not a split point.
        assert_eq!(
            parse_spdx_expression("MIT or Apache-2.0"),
            SpdxExpr::Single("MIT or Apache-2.0".to_string())
        );
    }

    #[test]
    fn ids_accessor_covers_all_variants() {
        assert_eq!(
            parse_spdx_expression("A OR B").ids(),
            &["A".to_string(), "B".to_string()]
        );
        assert_eq!(
            parse_spdx_expression("A AND B").ids(),
            &["A".to_string(), "B".to_string()]
        );
        assert_eq!(parse_spdx_expression("A").ids(), &["A".to_string()]);
    }
}
