//! The lexer that reduces a `custom_block` to its code, line by line.

use std::collections::{HashMap, VecDeque};

use super::names::{
    LevelKind, Scopes, assigned_names, bind_statement, capture_names, for_names, param_names,
};
use super::words::{HASH_AFTER, takes_operand};
use super::{UnreadReason, Unreadable, bracket_balance, is_local_name, is_word_char};

/// The code of one line of a `custom_block`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct CodeLine {
    /// The line with each literal that closes on it written as `""`, the
    /// part of a literal that stays open left out, and a comment cut.
    pub(super) text: String,
    /// The line ends inside a literal that closes on a later line.
    pub(super) open: bool,
}

/// A whole `custom_block` read by the lexer.
pub(super) struct Lexed {
    /// The code of every line.
    pub(super) lines: Vec<CodeLine>,
    /// Per line, whether it begins in code: outside every literal, heredoc
    /// body and `=begin` comment. The closing line of a `<<~` heredoc
    /// counts as code, since its indentation is free.
    pub(super) starts: Vec<bool>,
    /// The first place the block cannot be read with confidence.
    pub(super) unread: Option<Unreadable>,
}

/// A literal, or the code of an interpolation inside one, that the lexer
/// is in.
#[derive(Debug)]
enum Frame {
    /// A string, percent literal or regex, read up to `close`. A bracket
    /// pair nests, counted in `depth`.
    Literal {
        open: char,
        close: char,
        depth: usize,
        interpolates: bool,
        /// What the literal is and the line it opens on, for [`Unreadable`].
        kind: &'static str,
        line: usize,
        /// It was opened on its line after an ambiguous opener.
        guessed: bool,
        /// The text of a regex, kept for its named captures.
        body: Option<String>,
    },
    /// `#{…}` code, ended by the `}` that matches no `{` opened inside it.
    Interpolation { braces: usize },
}

/// A heredoc whose body begins on the line after its opener.
struct Heredoc {
    tag: String,
    /// Opened by `<<~` or `<<-`: the closing tag may be indented.
    indented: bool,
    /// Opened by `<<~`.
    squiggly: bool,
    /// The line of the opener.
    line: usize,
    /// It was opened on its line after an ambiguous opener.
    guessed: bool,
}

impl Heredoc {
    fn closes_on(&self, line: &str) -> bool {
        if self.indented {
            line.trim_start() == self.tag
        } else {
            line == self.tag
        }
    }
}

/// What the code before a `<<`, `%`, `?` or `/` makes of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Position {
    /// A value: the operator is a shift, a modulo, a ternary or a division.
    Value,
    /// A parameter's name, read as a value.
    Parameter,
    /// An operand position: the operator opens a literal.
    Operand,
    /// A word that may be a method or a local not seen assigned: the
    /// operator is read as an opener.
    Guess,
}

/// What carries from one line of a block to the next.
#[derive(Default)]
struct Carry {
    /// The open literals.
    stack: Vec<Frame>,
    /// The heredocs whose bodies are pending.
    heredocs: VecDeque<Heredoc>,
    /// The levels of nesting and the names bound in them.
    scopes: Scopes,
    /// The text of a block's parameter list from its opening `|`, while the
    /// list is still open.
    param_list: Option<String>,
    /// The heredoc a `<<` after a parameter's name would open.
    shifts: Vec<Heredoc>,
    /// The first line a guessed literal spans past.
    ambiguous: Option<Unreadable>,
    /// Inside a `=begin` … `=end` comment, opened on this line.
    comment: Option<usize>,
}

/// The code of every line of `block`, and the first place it cannot be
/// read with confidence.
pub(super) fn code_lines(block: &str) -> (Vec<CodeLine>, Option<Unreadable>) {
    let lexed = lex_block(block);
    (lexed.lines, lexed.unread)
}

/// True when `line` opens or closes a `=begin` comment: `marker` at the
/// start of the line, then whitespace or nothing.
fn is_comment_marker(line: &str, marker: &str) -> bool {
    line.strip_prefix(marker)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

/// Read every line of `block`.
pub(super) fn lex_block(block: &str) -> Lexed {
    let lines: Vec<&str> = block.lines().collect();
    let mut carry = Carry::default();
    let mut out = Vec::with_capacity(lines.len());
    let mut starts = Vec::with_capacity(lines.len());
    for (n, line) in lines.iter().enumerate() {
        if let Some(heredoc) = carry.heredocs.front() {
            let closes = heredoc.closes_on(line);
            starts.push(closes && heredoc.squiggly);
            if closes {
                carry.heredocs.pop_front();
            }
            out.push(CodeLine::default());
            continue;
        }
        if carry.comment.is_some() {
            if is_comment_marker(line, "=end") {
                carry.comment = None;
            }
            starts.push(false);
            out.push(CodeLine::default());
            continue;
        }
        if carry.stack.is_empty() && is_comment_marker(line, "=begin") {
            carry.comment = Some(n + 1);
            starts.push(false);
            out.push(CodeLine::default());
            continue;
        }
        starts.push(carry.stack.is_empty());
        out.push(lex_line(line, n + 1, &mut carry));
    }
    let unclosed = |line: usize, kind| Unreadable {
        line,
        kind,
        reason: UnreadReason::Unclosed,
    };
    let literal = carry.stack.first().and_then(|frame| match frame {
        Frame::Literal { kind, line, .. } => Some(unclosed(*line, *kind)),
        Frame::Interpolation { .. } => None,
    });
    let heredoc = carry.heredocs.front().map(|h| unclosed(h.line, "heredoc"));
    let comment = carry.comment.map(|line| unclosed(line, "comment"));
    // The last line each text appears on, as written and indented, so a
    // tag is looked up once per heredoc.
    let mut last_exact = HashMap::new();
    let mut last_indented = HashMap::new();
    for (n, line) in lines.iter().enumerate() {
        last_exact.insert(*line, n + 1);
        last_indented.insert(line.trim_start(), n + 1);
    }
    let shift = carry
        .shifts
        .iter()
        .find(|h| {
            let last = if h.indented {
                &last_indented
            } else {
                &last_exact
            };
            last.get(h.tag.as_str()).is_some_and(|&n| n > h.line)
        })
        .map(|h| Unreadable {
            line: h.line,
            kind: "heredoc",
            reason: UnreadReason::Ambiguous { operator: "<<" },
        });
    // On a tie the ambiguous reading names the cause of the open literal.
    let found = [carry.ambiguous, shift, literal, heredoc, comment];
    let unread = found
        .into_iter()
        .flatten()
        .min_by_key(|u| (u.line, matches!(u.reason, UnreadReason::Unclosed)));
    Lexed {
        lines: out,
        starts,
        unread,
    }
}

/// What the statement being read has opened so far.
#[derive(Default)]
struct Statement {
    /// Where it starts in the code of the line.
    start: usize,
    /// Its first assignment has been read.
    assigned: bool,
    /// Its first `in` has been read.
    read_in: bool,
    /// A `while`, `until` or `for` in it waits for the `do` that belongs to
    /// it, which opens no block.
    loop_do: bool,
    /// The level of the `def` it opened, and where the method's name starts
    /// in the code.
    def_level: Option<usize>,
    def_at: usize,
    /// The `=` after the method's name or parameter list has been read.
    def_body: bool,
    /// That `=` makes the method endless, so no `end` closes it.
    endless: bool,
    /// Where the parameter list of a `def` or `->` may start, whether it is
    /// a lambda's, and the `(` that opened it with the depth of brackets
    /// read inside it.
    params_from: Option<usize>,
    lambda: bool,
    params_open: Option<(usize, usize)>,
}

/// Open a `do … end` or `{ … }` block, binding the parameters a lambda
/// before it left `pending`.
fn open_block(scopes: &mut Scopes, pending: &mut Vec<String>) {
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
fn keyword(
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
fn end_statement(code: &str, st: &Statement, scopes: &mut Scopes) {
    bind_statement(code, scopes);
    if st.endless && st.def_level == Some(scopes.levels.len() - 1) {
        scopes.pop();
    }
}

/// Read line `number` of a block on top of what the lines before it left
/// in `carry`.
fn lex_line(line: &str, number: usize, carry: &mut Carry) -> CodeLine {
    let Carry {
        stack,
        heredocs,
        scopes,
        param_list,
        shifts,
        ambiguous,
        ..
    } = carry;
    let chars: Vec<char> = line.chars().collect();
    let mut text = String::new();
    let mut st = Statement::default();
    // The last character of code read, ignoring whitespace; `None` at the
    // start of a statement.
    let mut prev: Option<char> = None;
    // The word or number the code read so far ends with, with a leading
    // `@` or `$` and a trailing `?` or `!`, empty when it ends with
    // anything else; whether it is still being read; the word before it;
    // whether whitespace followed it; and the code character before it,
    // where a `.` makes it a method name.
    let mut word = String::new();
    let mut word_open = false;
    let mut prev_word = String::new();
    let mut spaced = false;
    let mut before_word: Option<char> = None;
    // Whether the word follows a `:`: `Some(false)` for a symbol (`:case`,
    // `a:case`), `Some(true)` for a constant path (`A::B`).
    let mut word_colon: Option<bool> = None;
    // The first ambiguous opener on this line: its operator and the kind
    // of literal it opened. Every literal opened after it is guessed.
    let mut guess: Option<(&'static str, &'static str)> = None;
    // The parameters of a lambda, bound once its body opens.
    let mut pending: Vec<String> = Vec::new();
    // The named captures of the regex literal that closed last.
    let mut captures: Vec<String> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if let Some(Frame::Literal {
            open,
            close,
            depth,
            interpolates,
            body,
            ..
        }) = stack.last_mut()
        {
            // A literal delimited by `\` (`%\a\`) has no escapes.
            if c == '\\' && *close != '\\' {
                if let Some(body) = body {
                    body.extend(&chars[i..(i + 2).min(chars.len())]);
                }
                i += 2;
            } else if *interpolates
                && c == '#'
                && chars.get(i + 1) == Some(&'$')
                && chars.get(i + 2).is_some_and(char::is_ascii_punctuation)
            {
                // `#$"` interpolates the special global `$"`.
                if let Some(body) = body {
                    body.extend(&chars[i..i + 3]);
                }
                i += 3;
            } else if *interpolates && c == '#' && chars.get(i + 1) == Some(&'{') {
                stack.push(Frame::Interpolation { braces: 0 });
                prev = None;
                word.clear();
                i += 2;
            } else {
                let mut closes = false;
                if c == *close {
                    if *depth == 0 {
                        closes = true;
                    } else {
                        *depth -= 1;
                    }
                } else if c == *open {
                    *depth += 1;
                }
                if !closes && let Some(body) = body {
                    body.push(c);
                }
                i += 1;
                if closes {
                    let frame = stack.pop();
                    if stack.is_empty() {
                        text.push_str("\"\"");
                        captures = match frame {
                            Some(Frame::Literal {
                                body: Some(body), ..
                            }) => capture_names(&body),
                            _ => Vec::new(),
                        };
                    }
                    prev = Some('"');
                    word.clear();
                }
            }
            continue;
        }
        let next = chars.get(i + 1).copied();
        let continues = is_word_char(c)
            && !spaced
            && prev.is_some_and(|p| is_word_char(p) || matches!(p, '@' | '$'))
            && (!word.starts_with(|d: char| d.is_ascii_digit())
                || number_continues(&word, c, next));
        // `empty?` and `save!` are one word.
        let joins = matches!(c, '?' | '!')
            && word_open
            && !spaced
            && prev.is_some_and(is_word_char)
            && next != Some('=');
        if word_open && !continues && !joins {
            word_open = false;
            let label = c == ':' && next != Some(':');
            keyword(
                &word,
                before_word,
                &prev_word,
                label,
                text.len(),
                &mut st,
                scopes,
                &mut pending,
            );
            prev_word.clone_from(&word);
        }
        // A special global (`$"`, `$/`, `$;`, `$-w`) is a value whose
        // punctuation opens nothing.
        if c == '$'
            && let Some(name) = next.filter(|n| n.is_ascii_punctuation() && *n != '_')
        {
            let len = if name == '-' { 3 } else { 2 };
            if stack.is_empty() {
                text.extend(&chars[i..(i + len).min(chars.len())]);
            }
            prev = Some('"');
            word.clear();
            i += len;
            continue;
        }
        let rest = &chars[i..];
        let shift = c == '<' && rest.get(1) == Some(&'<');
        // `:%` and `:/` are symbols, except that a `:` written against a
        // value may instead end a label or a ternary (`p:%`, `y ?x:%:a:`).
        let colon = i > 0 && chars[i - 1] == ':';
        let labelled = colon
            && i > 1
            && (is_word_char(chars[i - 2]) || matches!(chars[i - 2], '"' | '\'' | ')' | ']' | '}'));
        let symbol = colon && !labelled;
        let after_word = prev.is_some_and(|p| is_word_char(p) || matches!(p, '?' | '!'));
        let position = if !(shift || matches!(c, '%' | '?' | '/')) {
            Position::Value
        } else if labelled && matches!(c, '%' | '/') {
            Position::Guess
        } else if after_word && word_colon == Some(false) {
            Position::Value
        } else if after_word && word_colon == Some(true) {
            Position::Guess
        } else if (prev == Some('.') && !(i > 1 && chars[i - 2] == '.'))
            || (colon && i > 1 && chars[i - 2] == ':')
        {
            // `.<<`, `&.%`, `self./` and `a::<<` name a method.
            Position::Value
        } else if i > 1 && chars[i - 2] == '.' && matches!(prev, Some('%' | '/' | '<')) {
            // An operator written straight after such a method name
            // (`x.%%w`) is an operator.
            Position::Value
        } else {
            position(prev, &word, spaced, before_word == Some('.'), scopes)
        };
        let opens = matches!(position, Position::Operand | Position::Guess);
        // After a word, `/` followed by a space or `=` divides.
        let divides = after_word && matches!(next, None | Some(' ' | '\t' | '='));
        let mut heredoc = None;
        let opened = match c {
            '#' => break,
            '"' | '\'' => Some((1, Some(literal(c, c, c == '"', "string", number)))),
            // `` :` ``, `` .` `` and `` def ` `` name the backtick method.
            '`' if !colon && prev != Some('.') && !(after_word && word == "def") => {
                Some((1, Some(literal(c, c, true, "string", number))))
            }
            '%' if opens && !symbol => {
                percent_literal(rest, number, position == Position::Operand && !after_word)
                    .map(|(len, frame)| (len, Some(frame)))
            }
            // `then?"x"` is a method `then?`: the `?` joins the word.
            '?' if opens && !joins => char_literal_len(rest).map(|len| (len, None)),
            '/' if opens && !symbol && !divides => Some((1, Some(regex(number)))),
            '<' if shift && opens => heredoc_opener(&rest[2..], number).map(|(len, h)| {
                heredoc = Some(h);
                (2 + len, None)
            }),
            '<' if shift && position == Position::Parameter => {
                shifts.extend(heredoc_opener(&rest[2..], number).map(|(_, h)| h));
                None
            }
            _ => None,
        };
        if let Some((len, frame)) = opened {
            if position == Position::Guess && guess.is_none() {
                guess = match c {
                    '%' => Some(("%", "percent literal")),
                    '?' => Some(("?", "character literal")),
                    '<' => Some(("<<", "heredoc")),
                    '/' => Some(("/", "regex")),
                    _ => None,
                };
            }
            i += len;
            if let Some(mut h) = heredoc {
                h.guessed = guess.is_some();
                heredocs.push_back(h);
            }
            match frame {
                Some(mut frame) => {
                    if let Frame::Literal { guessed, .. } = &mut frame {
                        *guessed = guess.is_some();
                    }
                    stack.push(frame);
                }
                None => {
                    if stack.is_empty() {
                        text.push_str("\"\"");
                        captures.clear();
                    }
                    prev = Some('"');
                    word.clear();
                }
            }
            continue;
        }
        if let Some(Frame::Interpolation { braces }) = stack.last_mut() {
            match c {
                '{' => *braces += 1,
                '}' if *braces == 0 => {
                    stack.pop();
                    i += 1;
                    continue;
                }
                '}' => *braces -= 1,
                _ => {}
            }
        } else {
            text.push(c);
            let code = &text[st.start..text.len() - c.len_utf8()];
            if c == '='
                && !matches!(next, Some('=' | '~' | '>'))
                && !matches!(prev, Some('=' | '!' | '<' | '>'))
            {
                if st.def_level.is_some() && !st.def_body {
                    // The `=` of a setter's name (`def name=(v)`) or of an
                    // endless method (`def name(a) = a`) assigns nothing.
                    let header = &text[st.def_at..text.len() - 1];
                    if bracket_balance(header) == 0 {
                        st.def_body = true;
                        let name = header.trim();
                        let one_word = !name.is_empty() && !name.contains(char::is_whitespace);
                        let setter = !spaced && one_word && !name.contains('(');
                        st.endless = !setter && (name.ends_with(')') || one_word);
                    }
                } else {
                    if prev.is_some_and(is_word_char)
                        && before_word != Some('.')
                        && is_local_name(&word)
                    {
                        scopes.bind(&word, false);
                    }
                    if !st.assigned {
                        for name in assigned_names(code) {
                            scopes.bind(&name, false);
                        }
                    }
                    st.assigned = true;
                }
            } else if c == '=' && next == Some('~') && prev == Some('"') {
                for name in captures.drain(..) {
                    scopes.bind(&name, false);
                }
            } else if c == '>' && prev == Some('-') {
                st.params_from = Some(text.len());
                st.lambda = true;
                st.params_open = None;
            } else if c == ';' {
                end_statement(code, &st, scopes);
                st = Statement {
                    start: text.len(),
                    ..Statement::default()
                };
            } else if c.is_whitespace() && word == "in" && !st.read_in {
                for name in for_names(code) {
                    scopes.bind(&name, false);
                }
                st.read_in = true;
            } else if c == '{' {
                // After a method name or a call's `)` the brace opens a
                // block; anywhere else it opens a hash, whose names belong
                // to the scope around it.
                let block = match prev {
                    Some(')') => true,
                    Some(p) if is_word_char(p) || matches!(p, '?' | '!') => {
                        !HASH_AFTER.contains(&word.as_str())
                    }
                    _ => false,
                };
                if block {
                    open_block(scopes, &mut pending);
                } else {
                    scopes.push(LevelKind::Plain);
                }
            } else if c == '}' {
                scopes.pop();
            }
            if let Some(from) = st.params_from {
                read_params(c, &text, from, &mut st, scopes, &mut pending);
            }
            if c == '|' {
                if let Some(list) = param_list.take() {
                    for name in param_names(&list) {
                        scopes.bind(&name, true);
                    }
                } else if prev == Some('{') || word == "do" {
                    *param_list = Some(String::new());
                }
            } else if let Some(list) = param_list {
                list.push(c);
            }
        }
        if c.is_whitespace() {
            spaced = true;
        } else {
            if !continues && !joins {
                word.clear();
                before_word = prev;
                word_colon = (prev == Some(':')).then(|| i > 1 && chars[i - 2] == ':');
            }
            if matches!(c, '@' | '$') || is_word_char(c) || joins {
                word.push(c);
                word_open = stack.is_empty();
            }
            spaced = false;
            prev = if c == ';' { None } else { Some(c) };
        }
        i += 1;
    }
    if word_open {
        keyword(
            &word,
            before_word,
            &prev_word,
            false,
            text.len(),
            &mut st,
            scopes,
            &mut pending,
        );
    }
    if let Some(Frame::Literal {
        close: '\n',
        line: opened,
        ..
    }) = stack.last()
        && *opened < number
    {
        stack.pop();
        if stack.is_empty() {
            text.push_str("\"\"");
        }
    }
    end_statement(&text[st.start..], &st, scopes);
    if let Some(list) = param_list {
        list.push('\n');
    }
    if let Some((operator, kind)) = guess {
        let spans = stack
            .iter()
            .any(|f| matches!(f, Frame::Literal { guessed: true, .. }))
            || heredocs.iter().any(|h| h.guessed);
        if spans && ambiguous.is_none() {
            *ambiguous = Some(Unreadable {
                line: number,
                kind,
                reason: UnreadReason::Ambiguous { operator },
            });
        }
    }
    CodeLine {
        text,
        open: !stack.is_empty(),
    }
}

/// Read the code character `c`, just added to `text`, into the parameter
/// list of the `def` or `->` that may start at `from`, one character at a
/// time. Once the list closes its names are bound, or left `pending` for
/// the body of a lambda.
fn read_params(
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

/// What the code before a `<<`, `?`, `%` or `/` makes of it. `prev` is the
/// last code character (`"` for a literal that closed), `word` the word or
/// number the code ends with, `spaced` whether whitespace followed it, and
/// `scopes` the local variables and parameters visible here. A word after a
/// `.` (`method`) is a method name, which no local or keyword shadows.
///
/// A value ends with `)`, `]`, `}`, a closed literal or a number, or is a
/// word other than a keyword written straight against the operator
/// (`a%(b)`; `if%w[a]` opens a literal). These words count with a space
/// too: `self`, `nil`, `true`, `false`, `end`, `class` (for `class <<self`),
/// `def` (for `def /(other)`), a variable with a sigil (`@args`,
/// `$stdout`), a local and a parameter. A word [`takes_operand`] accepts,
/// an operator, `(`, `,` or the start of a statement leaves an operand
/// position, and any other word followed by a space a guess.
fn position(
    prev: Option<char>,
    word: &str,
    spaced: bool,
    method: bool,
    scopes: &Scopes,
) -> Position {
    match prev {
        Some(')' | ']' | '}' | '"') => Position::Value,
        Some(c) if is_word_char(c) || (matches!(c, '?' | '!') && word.ends_with(c)) => {
            let number = word.starts_with(|c: char| c.is_ascii_digit());
            if method && spaced && !number {
                return Position::Guess;
            }
            let bound = scopes.lookup(word);
            if !method && HASH_AFTER.contains(&word) {
                // A keyword takes an operand with or without a space
                // (`if%w[a]`).
                Position::Operand
            } else if !spaced
                || number
                || word.starts_with(['@', '$'])
                || matches!(
                    word,
                    "self" | "nil" | "true" | "false" | "end" | "class" | "def"
                )
                || bound == Some(false)
            {
                Position::Value
            } else if bound == Some(true) {
                Position::Parameter
            } else if takes_operand(word) {
                Position::Operand
            } else {
                Position::Guess
            }
        }
        _ => Position::Operand,
    }
}

fn literal(open: char, close: char, interpolates: bool, kind: &'static str, line: usize) -> Frame {
    Frame::Literal {
        open,
        close,
        depth: 0,
        interpolates,
        kind,
        line,
        guessed: false,
        body: None,
    }
}

/// A `/…/` regex opened on line `line`, which keeps its text.
fn regex(line: usize) -> Frame {
    let mut frame = literal('/', '/', true, "regex", line);
    if let Frame::Literal { body, .. } = &mut frame {
        *body = Some(String::new());
    }
    frame
}
/// The opener of a percent literal at the start of `text`, on line `line`:
/// its length and the literal it opens, or `None` when `text` opens none.
/// `bare` is whether it stands where an operand must and no word comes
/// before it (`x = %…`): only there does a bare `%` take a space, a tab,
/// `=` or the end of the line as its delimiter, which after a word make it
/// a modulo or a modulo assignment. A typed literal (`%q a `) takes a space
/// anywhere it opens.
fn percent_literal(text: &[char], line: usize, bare: bool) -> Option<(usize, Frame)> {
    let (kind, at) = match text.get(1) {
        Some(c @ ('q' | 'Q' | 'w' | 'W' | 'i' | 'I' | 'r' | 's' | 'x')) => (Some(*c), 2),
        _ => (None, 1),
    };
    let Some(&open) = text.get(at) else {
        // A literal delimited by the end of the line holds the next line.
        return (bare && kind.is_none())
            .then(|| (at, literal('\n', '\n', true, "percent literal", line)));
    };
    let close = match open {
        '(' => ')',
        '[' => ']',
        '{' => '}',
        '<' => '>',
        ' ' | '\t' | '=' if kind.is_none() && !bare => return None,
        ' ' | '\t' => open,
        c if c.is_ascii_punctuation() => c,
        _ => return None,
    };
    let interpolates = !matches!(kind, Some('q' | 'w' | 'i' | 's'));
    let mut frame = literal(open, close, interpolates, "percent literal", line);
    if kind == Some('r')
        && let Frame::Literal { body, .. } = &mut frame
    {
        *body = Some(String::new());
    }
    Some((at + 1, frame))
}

/// Whether `c` extends the number `word` (`0x1f`, `1_000`, `1e5`, `2r`):
/// a keyword written against a number starts a word of its own (`2if`).
fn number_continues(word: &str, c: char, next: Option<char>) -> bool {
    let hex = word.starts_with("0x") || word.starts_with("0X");
    c.is_ascii_digit()
        || c == '_'
        || (word == "0" && matches!(c, 'x' | 'X' | 'b' | 'B' | 'o' | 'O' | 'd' | 'D'))
        || (hex && c.is_ascii_hexdigit())
        || (!hex
            && matches!(c, 'e' | 'E')
            && next.is_some_and(|n| n.is_ascii_digit() || matches!(n, '-' | '+')))
        || (matches!(c, 'r' | 'i') && !next.is_some_and(is_word_char))
}

/// The length of a `?x` character literal at the start of `text`, or
/// `None` when the `?` is not one (`a ? b : c`, `?ab`).
fn char_literal_len(text: &[char]) -> Option<usize> {
    let c = *text.get(1).filter(|c| !c.is_whitespace())?;
    let len = if c == '\\' { 3 } else { 2 };
    // A word character followed straight by another is no literal (`?ab`),
    // and neither is a quote before a word (`?"a"`, a ternary before a
    // string); `?#if` is a literal before a modifier.
    if !matches!(c, '#' | '\\') && text.get(len).is_some_and(|&next| is_word_char(next)) {
        return None;
    }
    Some(len.min(text.len()))
}

/// The heredoc opened by the text after a `<<` on line `line`: the length
/// of that text and the heredoc, or `None` when the `<<` is a shift.
fn heredoc_opener(text: &[char], line: usize) -> Option<(usize, Heredoc)> {
    let indented = matches!(text.first(), Some('~' | '-'));
    let squiggly = text.first() == Some(&'~');
    let at = usize::from(indented);
    let first = *text.get(at)?;
    let (tag, end) = if matches!(first, '\'' | '"' | '`') {
        let len = text[at + 1..].iter().position(|&c| c == first)?;
        (&text[at + 1..at + 1 + len], at + len + 2)
    } else if first.is_ascii_alphabetic() || first == '_' {
        let len = text[at..].iter().take_while(|&&c| is_word_char(c)).count();
        (&text[at..at + len], at + len)
    } else {
        return None;
    };
    Some((
        end,
        Heredoc {
            tag: tag.iter().collect(),
            indented,
            squiggly,
            line,
            guessed: false,
        },
    ))
}
