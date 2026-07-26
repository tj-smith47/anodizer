//! Token machinery shared between block-level and positional rewrites.

use super::string_lit::{is_string_delim, raw_string_end};
use std::borrow::Cow;

/// A token from inside a `{{ }}` block.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Token {
    /// A bare identifier or dotted path (e.g., `Version`, `Env.VAR`).
    Ident(String),
    /// A quoted string literal including its delimiters (e.g., `"v"`,
    /// `'v'`, `` `v` ``).
    Quoted(String),
    /// A Tera array literal including brackets (e.g., `["a", "b", "c"]`).
    ArrayLiteral(String),
    /// A balanced Go sub-expression argument including its parentheses
    /// (e.g., `(base Path)`, `(trimprefix (base Path) "v")`).
    SubExpr(String),
    /// The pipe operator `|`.
    Pipe,
    /// Whitespace (preserved for reconstruction).
    Space(String),
    /// Anything else (parentheses, operators, etc.).
    Other(String),
}

/// Tokenize the inner content of a `{{ }}` block.
/// Splits into identifiers, quoted strings, pipes, spaces, and other chars.
pub(super) fn tokenize_block(inner: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let bytes = inner.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        // Whitespace
        if bytes[i].is_ascii_whitespace() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            tokens.push(Token::Space(inner[start..i].to_string()));
            continue;
        }

        // Quoted string
        if is_string_delim(bytes[i]) {
            let start = i;
            i = raw_string_end(bytes, i);
            tokens.push(Token::Quoted(inner[start..i].to_string()));
            continue;
        }

        // Array literal: `[...]` — capture the entire bracketed expression as one token.
        // This handles Tera array syntax like `["a", "b", "c"]`.
        if bytes[i] == b'[' {
            let start = i;
            let mut depth = 1;
            i += 1;
            while i < bytes.len() && depth > 0 {
                if bytes[i] == b'[' {
                    depth += 1;
                } else if bytes[i] == b']' {
                    depth -= 1;
                } else if is_string_delim(bytes[i]) {
                    // Skip quoted strings inside the array
                    i = raw_string_end(bytes, i);
                    continue;
                }
                i += 1;
            }
            tokens.push(Token::ArrayLiteral(inner[start..i].to_string()));
            continue;
        }

        // Parenthesis: either a named-arg call or a Go sub-expression.
        //
        // A `(` glued to the identifier before it opens Tera's named-arg call
        // syntax (`replace(s=…)`), which the rewrites deliberately leave alone.
        // A `(` anywhere else opens a Go sub-expression argument
        // (`trimprefix (base Path) "v"`), captured whole so the positional
        // rewriter can recurse into it as a single argument. An unclosed group
        // stays an `Other` char so no rewrite can consume the rest of the block.
        if bytes[i] == b'(' {
            if !matches!(tokens.last(), Some(Token::Ident(_)))
                && let Some(end) = balanced_paren_end(bytes, i)
            {
                tokens.push(Token::SubExpr(inner[i..end].to_string()));
                i = end;
                continue;
            }
            tokens.push(Token::Other("(".to_string()));
            i += 1;
            continue;
        }

        // Pipe
        if bytes[i] == b'|' {
            tokens.push(Token::Pipe);
            i += 1;
            continue;
        }

        // Identifier or dotted path (e.g., `Env.VAR`, `Version`)
        if bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' {
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'.')
            {
                i += 1;
            }
            tokens.push(Token::Ident(inner[start..i].to_string()));
            continue;
        }

        // Everything else (parentheses, operators, etc.)
        // Use chars().next() to handle multi-byte UTF-8 characters correctly.
        // Loop condition `i < inner.len()` guarantees `inner[i..]` is non-empty
        // so `chars().next()` always yields Some(_); the `break` is a
        // defensive no-op that keeps the function panic-free.
        let Some(ch) = inner[i..].chars().next() else {
            break;
        };
        tokens.push(Token::Other(ch.to_string()));
        i += ch.len_utf8();
    }

    tokens
}

/// Given `bytes[start] == b'('`, return the index just past its matching `)`,
/// or `None` when the group never closes.
///
/// String literals are skipped through [`raw_string_end`], so a parenthesis
/// inside a quoted string can never shift the nesting depth.
pub(super) fn balanced_paren_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth = depth.checked_sub(1)?;
                i += 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            b if is_string_delim(b) => i = raw_string_end(bytes, i),
            _ => i += 1,
        }
    }
    None
}

/// How a template block's parentheses fail to balance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ParenImbalance {
    /// `n` groups were opened and never closed before the block ended.
    Unclosed(usize),
    /// A `)` appeared with no group open.
    UnmatchedClose,
    /// A string literal ran to the end of the block, so everything after its
    /// opening delimiter — including any `)` — was read as string contents.
    /// Reported ahead of the paren counts because the odd delimiter is the
    /// cause and the paren count is only its symptom.
    UnterminatedString,
}

/// Report the parenthesis imbalance in one block's inner expression, if any.
///
/// Uses the same literal-skipping rule as [`balanced_paren_end`], so the
/// diagnostic and the sub-expression tokenizer can never disagree about which
/// parentheses are code.
pub(super) fn paren_imbalance(inner: &str) -> Option<ParenImbalance> {
    let bytes = inner.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => match depth.checked_sub(1) {
                Some(next) => {
                    depth = next;
                    i += 1;
                }
                None => return Some(ParenImbalance::UnmatchedClose),
            },
            delim if is_string_delim(delim) => {
                let end = raw_string_end(bytes, i);
                // `raw_string_end` returns the block end both for a literal
                // that closes on the very last byte and for one that never
                // closes; only the closing delimiter tells them apart.
                if end < i + 2 || bytes[end - 1] != delim {
                    return Some(ParenImbalance::UnterminatedString);
                }
                i = end;
            }
            _ => i += 1,
        }
    }
    (depth > 0).then_some(ParenImbalance::Unclosed(depth))
}

/// Collect non-whitespace tokens from a slice.
pub(super) fn significant_tokens(tokens: &[Token]) -> Vec<&Token> {
    tokens
        .iter()
        .filter(|t| !matches!(t, Token::Space(_)))
        .collect()
}

/// Convert a token back to its string representation.
pub(super) fn token_to_str(token: &Token) -> Cow<'_, str> {
    match token {
        Token::Ident(s)
        | Token::Quoted(s)
        | Token::ArrayLiteral(s)
        | Token::SubExpr(s)
        | Token::Space(s)
        | Token::Other(s) => Cow::Borrowed(s.as_str()),
        Token::Pipe => Cow::Borrowed("|"),
    }
}
