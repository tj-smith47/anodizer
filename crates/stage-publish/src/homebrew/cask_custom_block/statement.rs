//! What a statement of a `custom_block` has opened so far, and the words
//! and parameter lists that open or close its levels of nesting.

use super::is_word_char;
use super::names::{LevelKind, Scopes, bind_statement, param_names};

/// What the statement being read has opened so far.
#[derive(Default)]
pub(super) struct Statement {
    /// Where it starts in the code being read.
    pub(super) start: usize,
    /// Its first assignment has been read.
    pub(super) assigned: bool,
    /// Its first `in` has been read.
    pub(super) read_in: bool,
    /// A `while`, `until` or `for` in it waits for the `do` that belongs to
    /// it, which opens no block.
    pub(super) loop_do: bool,
    /// The level of the `def` it opened, and where the method's name starts
    /// in the code.
    pub(super) def_level: Option<usize>,
    pub(super) def_at: usize,
    /// The `=` after the method's name or parameter list has been read.
    pub(super) def_body: bool,
    /// That `=` makes the method endless, so no `end` closes it.
    pub(super) endless: bool,
    /// Where the parameter list of a `def` or `->` may start, whether it is
    /// a lambda's, and the `(` that opened it with the depth of brackets
    /// read inside it.
    pub(super) params_from: Option<usize>,
    pub(super) lambda: bool,
    pub(super) params_open: Option<(usize, usize)>,
}

/// Open a `do … end` or `{ … }` block, binding the parameters a lambda
/// before it left `pending`.
pub(super) fn open_block(scopes: &mut Scopes, pending: &mut Vec<String>) {
    scopes.push(LevelKind::Block);
    for name in pending.drain(..) {
        scopes.bind(&name, true);
    }
}

/// Apply what the word `word` just read opens or closes. `before` is the
/// code character before it, `prev_word` the word before it, `label`
/// whether a `:` follows it as a hash key, and `at` where the code after
/// it starts.
#[allow(clippy::too_many_arguments)]
pub(super) fn keyword(
    word: &str,
    before: Option<char>,
    prev_word: &str,
    label: bool,
    at: usize,
    st: &mut Statement,
    scopes: &mut Scopes,
    pending: &mut Vec<String>,
) {
    // `x.end`, `:end` and `end:` name a method, a symbol and a key.
    if label || matches!(before, Some('.' | ':')) {
        return;
    }
    match word {
        "do" if st.loop_do => st.loop_do = false,
        "do" => open_block(scopes, pending),
        "end" => scopes.pop(),
        "def" => {
            scopes.push(LevelKind::Gate);
            st.def_level = Some(scopes.levels.len() - 1);
            st.def_at = at;
            st.def_body = false;
            st.params_from = Some(at);
            st.lambda = false;
            st.params_open = None;
        }
        "class" | "module" => scopes.push(LevelKind::Gate),
        "begin" | "case" => scopes.push(LevelKind::Plain),
        "for" => {
            scopes.push(LevelKind::Plain);
            st.loop_do = true;
        }
        "if" | "unless" | "while" | "until" => {
            // After a value the word is a modifier (`x if y`), which no
            // `end` closes.
            let modifier = match before {
                None => false,
                Some(c) if is_word_char(c) || matches!(c, '?' | '!') => !matches!(
                    prev_word,
                    "else" | "then" | "do" | "begin" | "ensure" | "and" | "or" | "not"
                ),
                Some(c) => matches!(c, ')' | ']' | '}' | '"'),
            };
            if !modifier {
                scopes.push(LevelKind::Plain);
                st.loop_do = matches!(word, "while" | "until");
            }
        }
        _ => {}
    }
}

/// Close the statement `code`: bind the names it declares as a whole, and
/// close an endless method it defined.
pub(super) fn end_statement(code: &str, st: &Statement, scopes: &mut Scopes) {
    bind_statement(code, scopes);
    if st.endless && st.def_level == Some(scopes.levels.len() - 1) {
        scopes.pop();
    }
}

/// Read the code character `c`, just added to `text`, into the parameter
/// list of the `def` or `->` that may start at `from`, one character at a
/// time. Once the list closes its names are bound, or left `pending` for
/// the body of a lambda.
pub(super) fn read_params(
    c: char,
    text: &str,
    from: usize,
    st: &mut Statement,
    scopes: &mut Scopes,
    pending: &mut Vec<String>,
) {
    match st.params_open {
        None if c == '(' => {
            // `def name a, b = f(1)` has no parenthesized list.
            if text[from..text.len() - 1]
                .trim()
                .contains(char::is_whitespace)
            {
                st.params_from = None;
            } else {
                st.params_open = Some((text.len(), 1));
            }
        }
        None if c == ')' => st.params_from = None,
        None => {}
        Some((open, depth)) => {
            let depth = match c {
                '(' | '[' | '{' => depth + 1,
                ')' | ']' | '}' => depth - 1,
                _ => depth,
            };
            if depth > 0 {
                st.params_open = Some((open, depth));
                return;
            }
            let names = param_names(&text[open..text.len() - 1]);
            if st.lambda {
                pending.extend(names);
            } else {
                for name in names {
                    scopes.bind(&name, true);
                }
            }
            st.params_from = None;
            st.params_open = None;
        }
    }
}
