//! The lexer that reduces a `custom_block` to its code, line by line.

use std::collections::{HashMap, VecDeque};

use super::heredoc::{Heredoc, heredoc_opener, joins_next_line};
use super::literal::{Frame, literal, percent_literal, regex};
use super::names::{
    LevelKind, Scopes, assigned_names, bind_interpolated, bind_statement, capture_names, for_names,
    param_names,
};
use super::words::{
    HASH_AFTER, Position, char_literal_len, number_continues, operator_method_len, position,
    symbol_len,
};
use super::{UnreadReason, Unreadable, bracket_balance, is_local_name, is_word_char};

/// The code of one line of a `custom_block`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct CodeLine {
    /// The line with each literal that closes on it written as `""`, the
    /// part of a literal that stays open left out, and a comment cut.
    pub(super) text: String,
    /// The line ends inside a literal that closes on a later line.
    pub(super) open: bool,
    /// The code inside each `#{…}` that closes on the line, in the order
    /// they close, each its own list of statements. A `#{…}` that spans
    /// lines is one entry, on the line it closes on, its lines joined by
    /// `\n`.
    pub(super) interpolations: Vec<String>,
}

/// A whole `custom_block` read by the lexer.
pub(super) struct Lexed {
    /// The code of every line.
    pub(super) lines: Vec<CodeLine>,
    /// Per line, whether it begins in code: outside every literal, heredoc
    /// body and `=begin` comment. The closing line of a `<<~` heredoc
    /// counts as code, since its indentation is free.
    pub(super) starts: Vec<bool>,
    /// Per line that ends in code, outside every literal, heredoc body and
    /// `=begin` comment, the byte offset where its last literal ends, so
    /// only the whitespace past it is trailing. `None` for any other line.
    pub(super) code_ends: Vec<Option<usize>>,
    /// The first place the block cannot be read with confidence.
    pub(super) unread: Option<Unreadable>,
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
    /// The first heredoc body line that ends inside a `#{…}`.
    open_interpolation: Option<Unreadable>,
    /// Inside a `=begin` … `=end` comment, opened on this line.
    comment: Option<usize>,
    /// The statement a line ending in `\` continues onto the next line.
    continued: Option<Continued>,
}

/// What a line hands to the next one when the statement reads on past its
/// end: the line ends in a `\` continuation, in a `?\` character literal
/// that holds the newline, or in a `.`, `&.` or `::` before the method
/// name, or its last literal is closed by the newline (a `%r` closed that
/// way takes the letters that start the next line as its options). The
/// fields are the lexer's state at the end of the line, described where
/// [`lex_line`] reads them.
#[derive(Default)]
struct Continued {
    /// The code of the statement so far, which the positions in `st`
    /// index.
    text: String,
    st: Statement,
    prev: Option<char>,
    word: String,
    prev_word: String,
    before_word: Option<char>,
    word_colon: Option<bool>,
    pending: Vec<String>,
    captures: Vec<String>,
    /// A `.`, `&.` or `::` came last, so the next token is a method name
    /// whatever it looks like.
    method_next: bool,
    /// The method name that came last is spelled as an operator (`x.%`),
    /// so a `<<`, `%` or `/` written straight after it is an operator.
    after_operator_method: bool,
    /// The line ended in a `%r` closed by the newline, so the letters that
    /// start the next line are its options.
    regex_options: bool,
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
    let mut code_ends = Vec::with_capacity(lines.len());
    for (n, line) in lines.iter().enumerate() {
        code_ends.push(None);
        if let Some(heredoc) = carry.heredocs.front_mut() {
            let closes = heredoc.closes_on(line);
            starts.push(closes && heredoc.squiggly);
            if closes {
                carry.heredocs.pop_front();
                out.push(CodeLine::default());
            } else if heredoc.interpolates && line.contains("#{") {
                out.push(lex_body_line(line, n + 1, &mut carry));
            } else {
                out.push(CodeLine::default());
            }
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
        // A line that opens with the options of a regex closed by the
        // newline before it begins inside that literal.
        let options = carry.continued.as_ref().is_some_and(|c| c.regex_options)
            && line.starts_with(|c: char| c.is_ascii_alphabetic());
        starts.push(carry.stack.is_empty() && !options);
        let (code, end) = lex_line(line, n + 1, &mut carry);
        code_ends[n] = (!code.open).then_some(end);
        out.push(code);
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
    let found = [
        carry.ambiguous,
        shift,
        carry.open_interpolation,
        literal,
        heredoc,
        comment,
    ];
    let unread = found
        .into_iter()
        .flatten()
        .min_by_key(|u| (u.line, matches!(u.reason, UnreadReason::Unclosed)));
    Lexed {
        lines: out,
        starts,
        code_ends,
        unread,
    }
}

/// Read the `#{…}` code of line `number`, a body line of an interpolating
/// heredoc, and return it. A heredoc opened there has its body read from
/// the next line, before the rest of the body around it, as Ruby reads it.
fn lex_body_line(line: &str, number: usize, carry: &mut Carry) -> CodeLine {
    let depth = carry.stack.len();
    let queued = carry.heredocs.len();
    // The statement and parameter list the heredoc's opener left open go on
    // after the body.
    let continued = carry.continued.take();
    let param_list = carry.param_list.take();
    // The body as a literal that closes nowhere on the line, so only its
    // interpolations are read as code.
    carry
        .stack
        .push(literal('\n', '\n', true, "heredoc", number));
    let (code, _) = lex_line(line, number, carry);
    if carry.stack.len() > depth + 1 && carry.open_interpolation.is_none() {
        carry.open_interpolation = Some(Unreadable {
            line: number,
            kind: "heredoc",
            reason: UnreadReason::Unclosed,
        });
    }
    carry.stack.truncate(depth);
    let opened = carry.heredocs.len() - queued;
    carry.heredocs.rotate_right(opened);
    carry.continued = continued;
    carry.param_list = param_list;
    CodeLine { open: true, ..code }
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
/// in `carry`: its code, and the byte offset where the last literal read
/// on it ends, 0 when it has none.
fn lex_line(line: &str, number: usize, carry: &mut Carry) -> (CodeLine, usize) {
    let Carry {
        stack,
        heredocs,
        scopes,
        param_list,
        shifts,
        ambiguous,
        continued,
        ..
    } = carry;
    let chars: Vec<char> = line.chars().collect();
    // `text` is the code of the statement, from the line a `\` continued
    // when there is one; this line's code starts at `base`. `prev` is the
    // last character of code read, ignoring whitespace, `None` at the start
    // of a statement. `word` is the word or number the code read so far
    // ends with, with a leading `@` or `$` and a trailing `?` or `!`, empty
    // when it ends with anything else; `prev_word` the word before it, and
    // `before_word` the code character before it, where a `.` makes it a
    // method name. `word_colon` is whether the word follows a `:`:
    // `Some(false)` for a symbol (`:case`, `a:case`), `Some(true)` for a
    // constant path (`A::B`). `pending` holds the parameters of a lambda,
    // bound once its body opens, and `captures` the named captures of the
    // regex literal that closed last. `method_next` is whether a `.`, `&.`
    // or `::` came last, so the next token is a method name whatever it
    // looks like (`x. %`, `&.!~`), and `after_operator_method` whether such
    // a name spelled as an operator did.
    let Continued {
        mut text,
        mut st,
        mut prev,
        mut word,
        mut prev_word,
        mut before_word,
        mut word_colon,
        mut pending,
        mut captures,
        mut method_next,
        mut after_operator_method,
        regex_options,
    } = continued.take().unwrap_or_default();
    let base = text.len();
    // Whether the word is still being read, and whether whitespace
    // followed it; the `\` that continued a line counts as whitespace.
    let mut word_open = false;
    let mut spaced = base > 0;
    // The statement reads on into the next line: this one ends in `\`, a
    // `?\`, a `.` before the method name, or a literal the newline closes.
    let mut continues_line = false;
    // The first ambiguous opener on this line: its operator and the kind
    // of literal it opened. Every literal opened after it is guessed.
    let mut guess: Option<(&'static str, &'static str)> = None;
    // A `%r` closed by the newline before this line takes the letters it
    // starts with as its options (`%r` then `ab` then `ix`).
    let mut i = if regex_options {
        chars.iter().take_while(|c| c.is_ascii_alphabetic()).count()
    } else {
        0
    };
    // Past the last literal read, whose delimiter or content may be
    // whitespace that trimming the line must keep.
    let mut literal_end = i;
    let mut regex_options = false;
    // The code of each `#{…}` that closed on the line. It stays out of
    // `text`, whose names, brackets and `;` are the statement's.
    let mut interpolations = Vec::new();
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
                stack.push(Frame::Interpolation {
                    braces: 0,
                    code: String::new(),
                    start: 0,
                });
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
                    literal_end = i;
                    let frame = stack.pop();
                    close_literal(stack, &mut text);
                    if stack.is_empty() {
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
        if c == '\\' && next.is_none() && stack.is_empty() {
            continues_line = true;
            break;
        }
        // A comment between a `.` and the method name leaves it pending.
        let method_name = !c.is_whitespace() && c != '#' && std::mem::take(&mut method_next);
        let operator_method = !c.is_whitespace() && std::mem::take(&mut after_operator_method);
        if method_name && let Some(len) = operator_method_len(&chars[i..]) {
            emit(stack, &mut text, &chars[i..i + len]);
            prev = Some('"');
            word.clear();
            spaced = false;
            after_operator_method = true;
            i += len;
            continue;
        }
        // An operator symbol (`:+`, `:<=>`, `:[]=`) or a symbol of a name
        // ending in `=`, `?` or `!` is a value. A `:` after a value ends a
        // ternary instead (`f ? 1 :+ 2`).
        if c == ':'
            && (i == 0
                || !(is_word_char(chars[i - 1])
                    || matches!(chars[i - 1], ':' | '"' | '\'' | ')' | ']' | '}')))
            && !matches!(
                position(prev, &word, spaced, before_word == Some('.'), scopes),
                Position::Value | Position::Parameter
            )
            && let Some(len) = symbol_len(&chars[i + 1..])
        {
            emit(stack, &mut text, &chars[i..=i + len]);
            prev = Some('"');
            word.clear();
            spaced = false;
            i += len + 1;
            continue;
        }
        // A special global (`$"`, `$/`, `$;`, `$-w`) is a value whose
        // punctuation opens nothing. It is written as `$_` padded with `_`,
        // so a `;`, `,` or bracket in its name ends, continues or opens
        // nothing in the statement read from the code.
        if c == '$'
            && let Some(name) = next.filter(|n| n.is_ascii_punctuation() && *n != '_')
        {
            let len = if name == '-' { 3 } else { 2 }.min(chars.len() - i);
            let mut spelled = vec!['_'; len];
            spelled[0] = '$';
            emit(stack, &mut text, &spelled);
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
        // A `?` or `!` ends a word only as part of its name (`empty?`); one
        // read as an operator (`!/a/`) leaves an operand position.
        let after_word =
            prev.is_some_and(is_word_char) || (matches!(prev, Some('?' | '!')) && !word.is_empty());
        let position = if !(shift || matches!(c, '%' | '?' | '/')) {
            Position::Value
        } else if labelled && matches!(c, '%' | '/') {
            Position::Guess
        } else if after_word && word_colon == Some(false) {
            Position::Value
        } else if after_word && word_colon == Some(true) {
            Position::Guess
        } else if method_name {
            Position::Value
        } else if operator_method {
            // Written straight after an operator method name (`x.%%w`) the
            // operator is one, but a `?` opens a character literal; after a
            // space it may open the argument.
            if spaced {
                Position::Guess
            } else if c == '?' {
                Position::Operand
            } else {
                Position::Value
            }
        } else {
            let found = position(prev, &word, spaced, before_word == Some('.'), scopes);
            // Ruby reads a name that is no local variable as a method call,
            // and straight after one a `?` opens a character literal
            // (`EOS!?"`, `foo?=`, `x&.def??"`).
            if c == '?'
                && found == Position::Value
                && !spaced
                && after_word
                && names_a_call(&word, scopes)
            {
                Position::Operand
            } else {
                found
            }
        };
        let opens = matches!(position, Position::Operand | Position::Guess);
        // After a word, `/` followed by a space or `=` divides.
        let divides = after_word && matches!(next, None | Some(' ' | '\t' | '='));
        let mut heredoc = None;
        let opened = match c {
            '#' => break,
            '"' | '\'' => Some((1, Some(literal(c, c, c == '"', "string", number)))),
            // `` :` ``, `` .` `` and `` def ` `` name the backtick method.
            '`' if !colon && !(after_word && word == "def") => {
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
            literal_end = i;
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
                    close_literal(stack, &mut text);
                    if stack.is_empty() {
                        captures.clear();
                    }
                    prev = Some('"');
                    word.clear();
                }
            }
            // `?\` at the end of a line holds the newline, and the
            // statement reads on.
            if c == '?' && rest == ['?', '\\'] && stack.is_empty() {
                continues_line = true;
                break;
            }
            continue;
        }
        if let Some(Frame::Interpolation {
            braces,
            code,
            start,
        }) = stack.last_mut()
        {
            match c {
                '{' => *braces += 1,
                '}' if *braces == 0 => {
                    bind_interpolated(&code[*start..], scopes);
                    interpolations.push(std::mem::take(code));
                    stack.pop();
                    i += 1;
                    continue;
                }
                '}' => *braces -= 1,
                // Ruby binds the names a statement inside `#{…}` assigns
                // in the scope around the literal.
                ';' => {
                    bind_interpolated(&code[*start..], scopes);
                    *start = code.len() + 1;
                }
                _ => {}
            }
            code.push(c);
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
                    // `-> { … }` is the body of a lambda.
                    Some('>') => text[..text.len() - 1].trim_end().ends_with("->"),
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
                // A `::` that ended an earlier line, or stood before a
                // space, makes a constant path too (`A::\nB`).
                word_colon =
                    (prev == Some(':')).then(|| method_name || (i > 1 && chars[i - 2] == ':'));
            }
            if matches!(c, '@' | '$') || is_word_char(c) || joins {
                word.push(c);
                word_open = stack.is_empty();
            }
            spaced = false;
            method_next = (c == '.' && (i == 0 || chars[i - 1] != '.') && next != Some('.'))
                || (c == ':' && i > 0 && chars[i - 1] == ':' && (i < 2 || chars[i - 2] != ':'));
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
    // A literal delimited by the end of a line closes at the end of the
    // next, unless an odd run of `\` there joins the line after it. The
    // newline that closes it ends no statement, so the next line reads on
    // after the value.
    if let Some(Frame::Literal {
        close: '\n',
        line: opened,
        body,
        ..
    }) = stack.last()
        && *opened < number
        && !joins_next_line(line)
    {
        regex_options = body.is_some();
        stack.pop();
        literal_end = chars.len();
        close_literal(stack, &mut text);
        if stack.is_empty() {
            prev = Some('"');
            word.clear();
            continues_line = true;
        }
    }
    // A line ending in `.`, `&.` or `::` names its method on a later line,
    // past any blank or comment line, so the statement reads on.
    if stack.is_empty() && method_next {
        continues_line = true;
    }
    // A `#{…}` open at the end of the line ends a statement there.
    if let Some(Frame::Interpolation { code, start, .. }) = stack.last_mut() {
        bind_interpolated(&code[*start..], scopes);
        code.push('\n');
        *start = code.len();
    }
    let mut code = text[base..].to_string();
    if continues_line {
        code.push('\\');
        text.push(' ');
        *continued = Some(Continued {
            text,
            st,
            prev,
            word,
            prev_word,
            before_word,
            word_colon,
            pending,
            captures,
            method_next,
            after_operator_method,
            regex_options,
        });
    } else {
        end_statement(&text[st.start..], &st, scopes);
    }
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
    let end = chars[..literal_end.min(chars.len())]
        .iter()
        .map(|c| c.len_utf8())
        .sum();
    let code = CodeLine {
        text: code,
        open: !stack.is_empty(),
        interpolations,
    };
    (code, end)
}

/// True when the word `word` is a name Ruby reads as a method call: no
/// number, variable with a sigil, local variable, parameter or keyword
/// that ends a value.
fn names_a_call(word: &str, scopes: &Scopes) -> bool {
    !word.starts_with(|c: char| c.is_ascii_digit() || matches!(c, '@' | '$'))
        && !matches!(
            word,
            "self" | "nil" | "true" | "false" | "end" | "__FILE__" | "__LINE__" | "__ENCODING__"
        )
        && scopes.lookup(word).is_none()
}

/// Add the code `code` to `text`, or to the `#{…}` being read.
fn emit(stack: &mut [Frame], text: &mut String, code: &[char]) {
    match stack.last_mut() {
        Some(Frame::Interpolation { code: inner, .. }) => inner.extend(code),
        _ => text.extend(code),
    }
}

/// Write a literal that just closed as `""`, in `text` when it was read in
/// code or in the `#{…}` it was read in.
fn close_literal(stack: &mut [Frame], text: &mut String) {
    emit(stack, text, &['"', '"']);
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
