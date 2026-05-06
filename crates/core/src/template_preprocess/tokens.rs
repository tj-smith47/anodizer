//! Token machinery shared between block-level and positional rewrites.

use std::borrow::Cow;

/// A token from inside a `{{ }}` block.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Token {
    /// A bare identifier or dotted path (e.g., `Version`, `Env.VAR`).
    Ident(String),
    /// A quoted string literal including its quotes (e.g., `"v"`).
    Quoted(String),
    /// A Tera array literal including brackets (e.g., `["a", "b", "c"]`).
    ArrayLiteral(String),
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
        if bytes[i] == b'"' || bytes[i] == b'\'' {
            let quote = bytes[i];
            let start = i;
            i += 1;
            while i < bytes.len() && bytes[i] != quote {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            if i < bytes.len() {
                i += 1; // closing quote
            }
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
                } else if bytes[i] == b'"' || bytes[i] == b'\'' {
                    // Skip quoted strings inside the array
                    let quote = bytes[i];
                    i += 1;
                    while i < bytes.len() && bytes[i] != quote {
                        if bytes[i] == b'\\' && i + 1 < bytes.len() {
                            i += 2;
                        } else {
                            i += 1;
                        }
                    }
                    if i < bytes.len() {
                        i += 1; // closing quote
                    }
                    continue;
                }
                i += 1;
            }
            tokens.push(Token::ArrayLiteral(inner[start..i].to_string()));
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
        | Token::Space(s)
        | Token::Other(s) => Cow::Borrowed(s.as_str()),
        Token::Pipe => Cow::Borrowed("|"),
    }
}
