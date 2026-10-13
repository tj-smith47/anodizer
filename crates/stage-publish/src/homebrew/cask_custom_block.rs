//! What a cask's `custom_block` says about its Linux side and its OS
//! dependencies, the two facts that decide whether the rendered cask
//! carries `depends_on :macos`.

use std::collections::{HashMap, HashSet, VecDeque};

use anodizer_core::log::StageLogger;

use super::cask::CaskParams;

/// What a `custom_block` contributes to the `depends_on :macos` decision.
#[derive(Default, Debug, PartialEq, Eq)]
pub(super) struct CustomBlockOsFacts {
    /// The word `on_linux`, `on_system` or the Linux-only `app_image`
    /// appears in the code of the block: the cask has a Linux side.
    pub(super) other_os_block: bool,
    /// A `depends_on` statement names an OS: the key `macos:`,
    /// `maximum_macos:` or `linux:`, or the symbol `:macos` or `:linux`.
    pub(super) os_dependency: bool,
    /// The first place the lexer could not read with confidence. When it is
    /// set, both facts above come from a word scan of the raw text.
    pub(super) unread: Option<Unreadable>,
}

/// A place in a `custom_block` past which its code cannot be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Unreadable {
    /// The line of the block it is on, counted from 1.
    pub(super) line: usize,
    /// The literal involved: `string`, `heredoc`, `percent literal`,
    /// `regex` or `character literal`.
    pub(super) kind: &'static str,
    /// Why the code after it cannot be trusted.
    pub(super) reason: UnreadReason,
}

/// Why a `custom_block` could not be read past a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UnreadReason {
    /// The literal opened there is still open after the last line.
    Unclosed,
    /// The `operator` there reads as an operator or as the opener of a
    /// literal depending on whether the word before it is a local variable,
    /// which the lexer cannot tell, and the literal it would open spans
    /// more than one line.
    Ambiguous { operator: &'static str },
}

/// Read a `custom_block` for the facts `Homebrew/OSDependsOn` and
/// `Cask#supports_linux?` look at.
///
/// The block is first reduced to its code, line by line, and both facts
/// read only that. These are set aside, and a literal that does not close
/// on its line carries on into the following lines until it closes:
///
/// - a `"…"` or `'…'` string; inside a `"…"` string, `#{…}` is read as code
///   with its own braces and strings, so the string closes at its own `"`;
/// - a heredoc body, opened by `<<~TAG`, `<<-TAG` or `<<TAG` with the tag
///   bare or quoted and closed by a line holding the tag alone (indented
///   for `<<~` and `<<-`), case-sensitive; `<<` followed by a space or a
///   digit is a shift;
/// - a `%(…)`, `%q`, `%Q`, `%w`, `%W`, `%i`, `%I`, `%r`, `%s` or `%x`
///   literal delimited by `()`, `[]`, `{}`, `<>` or one repeated
///   punctuation character;
/// - a `?x` character literal (`?#`, `?"`, `?'`) that no word follows
///   straight away;
/// - a `/…/` regex where the `/` follows `=~`, `!~`, `(`, `,` or `=`, or
///   starts a line or a `;` statement;
/// - a `#` comment, which cuts the rest of the line.
///
/// A `<<`, `%` or `?` that follows a value is a shift, a modulo or a
/// ternary and opens nothing. The code before it ends with a value when,
/// whitespace aside, it ends with `)`, `]`, `}`, a number or a closed
/// literal, or with one of these words:
///
/// - `self`, `nil`, `true`, `false`, `end`, or `class` (`class <<self`);
/// - an instance, class or global variable (`@args`, `@@all`, `$stdout`);
/// - a local variable, which is a name assigned earlier in the block:
///   `name =`, `name ||=` and the other operator assignments (`==`, `=~`,
///   `=>`, `!=`, `<=` and `>=` assign nothing, and neither does
///   `obj.name =`), `a, b =`, `(a, b) =`, `for a in`, `rescue => a`;
/// - a parameter: of a block (`do |a, b|`, `{ |a| … }`, a list spread over
///   several lines), of a method (`def name(a, b = 1, *rest, **kw, &blk)`,
///   with or without parentheses) or of a lambda (`->(x)`, `-> (x)`);
/// - any word written straight against the operator (`a%(b)`, `1<<FLAGS`).
///
/// It does not after an operator, `(`, `,`, `;`, the start of a line, a
/// Ruby keyword, a cask stanza or common method that takes an argument
/// (`caveats <<~EOS`, `puts %w[a]`), and there the operator opens a
/// literal. After any other word followed by a space the lexer cannot tell
/// a method from a local it did not see assigned, so it reads the operator
/// as an opener; when the literal it opens spans more than one line, the
/// block is unreadable from that line.
///
/// Then:
///
/// - A Linux side holds when `on_linux`, `on_system` or `app_image` appears
///   as a whole word, so a receiver (`self.on_linux do`), a parenthesis or
///   a one-line nesting still counts.
/// - A `depends_on` statement starts where `depends_on` is the first word
///   of a line or of a `;`-separated statement, after any opening `(`. It
///   continues onto the next line while the current one ends with `,`,
///   `(`, `[`, `{`, `|`, `.`, `&&`, `||`, `=>` or `\`, ends inside an open
///   literal, or while the statement has opened more `(`, `[` and `{` than
///   it has closed. A blank line inside it is passed over, and a line that
///   opens with `depends_on` starts a new statement.
/// - An OS dependency holds when the joined statement names an OS key or
///   symbol as a whole word.
///
/// The block is unreadable from a line when a literal opened there is
/// still open after the last line, when a literal opened there from an
/// ambiguous position spans several lines, or when a parameter's name is
/// followed there by a `<<` whose heredoc tag closes on a later line (a
/// parameter outside its block is no local). Both facts then come from a
/// word scan of the raw text with `#` comments cut (a `#` that starts a
/// line or follows whitespace): a Linux side when one of its words appears,
/// an OS dependency when `depends_on` and an OS key or symbol both do.
/// [`CustomBlockOsFacts::unread`] names the line, for the warning
/// [`warn_unread_custom_block`] prints.
// A wrong verdict renders a cask Homebrew refuses. A missed Linux side or a
// missed OS dependency adds `depends_on :macos`: beside a real `on_linux` a
// Linux install stops at "This cask requires macOS.", and beside the
// block's own `depends_on macos: :ventura` Homebrew 7.0.8 refuses to load
// the cask on every system (`brew readall --os=all --arch=all`: "Calling
// combining `depends_on :macos` with a versioned macOS dependency is
// disabled! Use only the versioned macOS dependency instead."; two
// `depends_on macos:` lines fail the same way with "`depends_on macos:`
// cannot be combined with another macOS `depends_on`"). A Linux side or OS
// dependency read where there is none leaves `depends_on :macos` out, and a
// cask whose downloads all sit in `on_macos` then has a nil Linux `sha256`
// and fails to load on Linux. The word scan errs toward that second case,
// which a cask with no Linux side meets only when its own text names one.
//
// Not read correctly: a stanza built at runtime (through `send` or
// `define_method`); a `depends_on` after a `do` on the same line; a regex
// after any token not listed above, which is read as code; and an
// ambiguous opener whose literal closes on the same line, whose content is
// set aside without a warning. A local assigned inside a method and used
// after it still counts as one, and so does a parameter used outside its
// block before a `%` or `?`.
// `depends_on_macos:` overrides the verdict for every case.
pub(super) fn custom_block_os_facts(block: Option<&str>) -> CustomBlockOsFacts {
    let Some(block) = block else {
        return CustomBlockOsFacts::default();
    };
    let (code, unread) = code_lines(block);
    if let Some(unread) = unread {
        let text = without_comments(block);
        return CustomBlockOsFacts {
            other_os_block: names_a_linux_side(&text),
            os_dependency: text
                .split(|c: char| !is_word_char(c))
                .any(|w| w == "depends_on")
                && names_an_os(&text),
            unread: Some(unread),
        };
    }
    let other_os_block = code.iter().any(|line| names_a_linux_side(&line.text));
    let os_dependency = depends_on_statements(&code)
        .iter()
        .any(|statement| names_an_os(statement));
    CustomBlockOsFacts {
        other_os_block,
        os_dependency,
        unread: None,
    }
}

/// Warn through `log` when the `custom_block` of the cask `params` render
/// could not be read to its end and no `depends_on_macos:` decides the
/// verdict in its place.
pub(super) fn warn_unread_custom_block(params: &CaskParams<'_>, log: &StageLogger) {
    if params.depends_on_macos.is_some() {
        return;
    }
    let Some(Unreadable { line, kind, reason }) = custom_block_os_facts(params.custom_block).unread
    else {
        return;
    };
    let cause = match reason {
        UnreadReason::Unclosed => format!("an unclosed {kind}"),
        UnreadReason::Ambiguous { operator } => {
            format!("the '{operator}' there could open a {kind} or be an operator")
        }
    };
    log.warn(&format!(
        "homebrew cask '{}': custom_block could not be read past line {line} ({cause}); \
         the Linux-side and depends_on checks fell back to a word scan, which may over-match — \
         set depends_on_macos: to decide it",
        params.name
    ));
}

/// `block` with each `#` comment cut: from a `#` that starts a line or
/// follows whitespace to the end of that line.
fn without_comments(block: &str) -> String {
    block
        .lines()
        .map(|line| {
            let mut prev_space = true;
            for (at, c) in line.char_indices() {
                if c == '#' && prev_space {
                    return &line[..at];
                }
                prev_space = c.is_whitespace();
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// True when `on_linux`, `on_system` or `app_image` appears in `text` as a
/// whole word.
fn names_a_linux_side(text: &str) -> bool {
    text.split(|c: char| !is_word_char(c))
        .any(|w| matches!(w, "on_linux" | "on_system" | "app_image"))
}

/// True when `name` can be a local variable: it starts with a lowercase
/// letter or `_`.
fn is_local_name(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
}

/// Words after which a `<<`, `%` or `?` opens a literal: Ruby keywords, the
/// cask stanzas and the common methods that take an argument.
const OPERAND_WORDS: &[&str] = &[
    "and",
    "app",
    "artifact",
    "begin",
    "binary",
    "break",
    "case",
    "caveats",
    "conflicts_with",
    "depends_on",
    "desc",
    "do",
    "else",
    "elsif",
    "homepage",
    "if",
    "in",
    "manpage",
    "name",
    "next",
    "not",
    "or",
    "p",
    "postflight",
    "preflight",
    "print",
    "puts",
    "raise",
    "require",
    "return",
    "system",
    "then",
    "uninstall",
    "uninstall_postflight",
    "uninstall_preflight",
    "unless",
    "until",
    "url",
    "warn",
    "when",
    "while",
    "yield",
    "zap",
];

/// The code of one line of a `custom_block`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct CodeLine {
    /// The line with each literal that closes on it written as `""`, the
    /// part of a literal that stays open left out, and a comment cut.
    text: String,
    /// The line ends inside a literal that closes on a later line.
    open: bool,
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
    },
    /// `#{…}` code, ended by the `}` that matches no `{` opened inside it.
    Interpolation { braces: usize },
}

/// A heredoc whose body begins on the line after its opener.
struct Heredoc {
    tag: String,
    indented: bool,
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

/// What the code before a `<<`, `%` or `?` makes of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Position {
    /// A value: the operator is a shift, a modulo or a ternary.
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
    /// The local variables assigned so far.
    locals: HashSet<String>,
    /// The parameters of the blocks, methods and lambdas read so far.
    params: HashSet<String>,
    /// The text of a block's parameter list from its opening `|`, while the
    /// list is still open.
    param_list: Option<String>,
    /// The heredoc a `<<` after a parameter's name would open.
    shifts: Vec<Heredoc>,
    /// The first line a guessed literal spans past.
    ambiguous: Option<Unreadable>,
}

/// The code of every line of `block`, and the first place it cannot be
/// read with confidence.
fn code_lines(block: &str) -> (Vec<CodeLine>, Option<Unreadable>) {
    let lines: Vec<&str> = block.lines().collect();
    let mut carry = Carry::default();
    let mut out = Vec::new();
    for (n, line) in lines.iter().enumerate() {
        if let Some(heredoc) = carry.heredocs.front() {
            if heredoc.closes_on(line) {
                carry.heredocs.pop_front();
            }
            out.push(CodeLine::default());
            continue;
        }
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
    let found = [carry.ambiguous, shift, literal, heredoc];
    let first = found
        .into_iter()
        .flatten()
        .min_by_key(|u| (u.line, matches!(u.reason, UnreadReason::Unclosed)));
    (out, first)
}

/// Read line `number` of a block on top of what the lines before it left
/// in `carry`.
fn lex_line(line: &str, number: usize, carry: &mut Carry) -> CodeLine {
    let Carry {
        stack,
        heredocs,
        locals,
        params,
        param_list,
        shifts,
        ambiguous,
    } = carry;
    let chars: Vec<char> = line.chars().collect();
    let mut text = String::new();
    // Where the statement being read starts in `text`; whether its first
    // assignment and its first `in` have been read; and where the parameter list of a `def` or
    // `->` in it may start.
    let mut statement = 0;
    let mut assigned = false;
    let mut read_in = false;
    let mut params_from: Option<usize> = None;
    // The last two characters of code read, ignoring whitespace; `None`
    // at the start of a statement.
    let mut prev: Option<char> = None;
    let mut before_prev: Option<char> = None;
    // The word or number the code read so far ends with, with a leading
    // `@` or `$`, empty when it ends with anything else; whether
    // whitespace followed it; and the code character before it, where a
    // `.` makes it a method name.
    let mut word = String::new();
    let mut spaced = false;
    let mut before_word: Option<char> = None;
    // The first ambiguous opener on this line: its operator and the kind
    // of literal it opened. Every literal opened after it is guessed.
    let mut guess: Option<(&'static str, &'static str)> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if let Some(Frame::Literal {
            open,
            close,
            depth,
            interpolates,
            ..
        }) = stack.last_mut()
        {
            if c == '\\' {
                i += 2;
            } else if *interpolates && c == '#' && chars.get(i + 1) == Some(&'{') {
                stack.push(Frame::Interpolation { braces: 0 });
                prev = None;
                word.clear();
                i += 2;
            } else {
                if c == *close {
                    if *depth == 0 {
                        stack.pop();
                        if stack.is_empty() {
                            text.push_str("\"\"");
                        }
                        (before_prev, prev) = (prev, Some('"'));
                        word.clear();
                    } else {
                        *depth -= 1;
                    }
                } else if c == *open {
                    *depth += 1;
                }
                i += 1;
            }
            continue;
        }
        let position = position(prev, &word, spaced, locals, params);
        let opens = matches!(position, Position::Operand | Position::Guess);
        let rest = &chars[i..];
        let shift = c == '<' && rest.get(1) == Some(&'<');
        let mut heredoc = None;
        let opened = match c {
            '#' => break,
            '"' | '\'' => Some((1, Some(literal(c, c, c == '"', "string", number)))),
            '%' if opens => percent_literal(rest, number).map(|(len, frame)| (len, Some(frame))),
            '?' if opens => char_literal_len(rest).map(|len| (len, None)),
            '/' if prev.is_none()
                || matches!(prev, Some('(' | ',' | '='))
                || (prev == Some('~') && matches!(before_prev, Some('=' | '!'))) =>
            {
                Some((1, Some(literal('/', '/', true, "regex", number))))
            }
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
                    }
                    (before_prev, prev) = (prev, Some('"'));
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
            let code = &text[statement..text.len() - c.len_utf8()];
            if c == '='
                && !matches!(rest.get(1), Some('=' | '~' | '>'))
                && !matches!(prev, Some('=' | '!' | '<' | '>'))
            {
                if prev.is_some_and(is_word_char)
                    && before_word != Some('.')
                    && is_local_name(&word)
                {
                    locals.insert(word.clone());
                }
                if !assigned {
                    locals.extend(assigned_names(code));
                }
                assigned = true;
            } else if c == ')' {
                if let Some(from) = params_from
                    && let Some(names) = parenthesized_params(&text[from..])
                {
                    params.extend(names);
                    params_from = None;
                }
            } else if c == '>' && prev == Some('-') {
                params_from = Some(text.len());
            } else if c == ';' {
                bind_statement(code, locals, params);
                statement = text.len();
                assigned = false;
                read_in = false;
                params_from = None;
            } else if c.is_whitespace() && word == "def" && code.trim_start() == "def" {
                params_from = Some(text.len());
            } else if c.is_whitespace() && word == "in" && !read_in {
                locals.extend(for_names(code));
                read_in = true;
            }
        }
        if c == '|' {
            if let Some(list) = param_list.take() {
                params.extend(param_names(&list));
            } else if prev == Some('{') || word == "do" {
                *param_list = Some(String::new());
            }
        } else if let Some(list) = param_list {
            list.push(c);
        }
        if c.is_whitespace() {
            spaced = true;
        } else {
            let sigil = matches!(c, '@' | '$');
            let continues = is_word_char(c)
                && !spaced
                && prev.is_some_and(|p| is_word_char(p) || matches!(p, '@' | '$'));
            if !continues {
                word.clear();
                before_word = prev;
            }
            if sigil || is_word_char(c) {
                word.push(c);
            }
            spaced = false;
            (before_prev, prev) = if c == ';' {
                (None, None)
            } else {
                (prev, Some(c))
            };
        }
        i += 1;
    }
    bind_statement(&text[statement..], locals, params);
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

/// What the code before a `<<`, `?` or `%` makes of it. `prev` is the last
/// code character (`"` for a literal that closed), `word` the word or
/// number the code ends with, `spaced` whether whitespace followed it, and
/// `locals` and `params` the local variables and parameters read so far.
///
/// A value ends with `)`, `]`, `}`, a closed literal or a number, or is a
/// word written straight against the operator (`a%(b)`). These words count
/// with a space too: `self`, `nil`, `true`, `false`, `end`, `class` (for
/// `class <<self`), a variable with a sigil (`@args`, `$stdout`), a local
/// and a parameter. A word in [`OPERAND_WORDS`], an operator, `(`, `,` or
/// the start of a statement leaves an operand position, and any other word
/// followed by a space a guess.
fn position(
    prev: Option<char>,
    word: &str,
    spaced: bool,
    locals: &HashSet<String>,
    params: &HashSet<String>,
) -> Position {
    match prev {
        Some(')' | ']' | '}' | '"') => Position::Value,
        Some(c) if is_word_char(c) => {
            if !spaced
                || word.starts_with(|c: char| c.is_ascii_digit() || matches!(c, '@' | '$'))
                || matches!(word, "self" | "nil" | "true" | "false" | "end" | "class")
                || locals.contains(word)
            {
                Position::Value
            } else if params.contains(word) {
                Position::Parameter
            } else if OPERAND_WORDS.contains(&word) {
                Position::Operand
            } else {
                Position::Guess
            }
        }
        _ => Position::Operand,
    }
}

/// The names `code`, the statement before an `=`, assigns: `a` from `a`
/// or `a ||` (and the other operator assignments), `a`, `b` and `c` from
/// `a, *b, c` and `(a, b)`. Empty when `code` is anything else.
fn assigned_names(code: &str) -> Vec<String> {
    let target = code
        .trim()
        .trim_end_matches(['|', '&', '+', '-', '*', '/', '%'])
        .trim();
    let target = target
        .strip_prefix('(')
        .and_then(|t| t.strip_suffix(')'))
        .unwrap_or(target);
    let names: Vec<&str> = target
        .split(',')
        .map(|name| name.trim().trim_start_matches('*'))
        .collect();
    if names
        .iter()
        .all(|name| is_local_name(name) && name.chars().all(is_word_char))
    {
        names.into_iter().map(String::from).collect()
    } else {
        Vec::new()
    }
}

/// The names a `for … in` statement `code`, read up to `in`, binds.
fn for_names(code: &str) -> Vec<String> {
    code.trim_start()
        .strip_prefix("for ")
        .and_then(|rest| rest.trim_end().strip_suffix(" in"))
        .map(|names| assigned_names(names.trim()))
        .unwrap_or_default()
}

/// Bind the names a whole statement `code` declares: the variable of a
/// `rescue … => e`, and the parameters of a `def name a, b` written
/// without parentheses.
fn bind_statement(code: &str, locals: &mut HashSet<String>, params: &mut HashSet<String>) {
    let code = code.trim();
    if code.starts_with("rescue")
        && let Some((_, name)) = code.rsplit_once("=>")
    {
        locals.extend(assigned_names(name));
    }
    if let Some(rest) = code.strip_prefix("def ")
        && let Some((name, list)) = rest.trim_start().split_once(char::is_whitespace)
        && !name.contains('(')
    {
        params.extend(param_names(list));
    }
}

/// The parameters in `code`, the text after a `def` or `->` up to a `)`
/// just read: `None` while that `)` leaves the list open, the names once it
/// closes the list, and no names when `code` starts no parenthesized list
/// (`def name a, b`).
fn parenthesized_params(code: &str) -> Option<Vec<String>> {
    let Some(open) = code.find('(') else {
        return Some(Vec::new());
    };
    if code[..open].trim().contains(char::is_whitespace) {
        return Some(Vec::new());
    }
    let list = &code[open..];
    (bracket_balance(list) == 0).then(|| param_names(&list[1..list.len() - 1]))
}

/// The names in a parameter list: each comma-separated entry's leading
/// name after any `*`, `**`, `&` or `(`, so a default value or a keyword's
/// `:` is passed over.
fn param_names(list: &str) -> Vec<String> {
    list.split(',')
        .filter_map(|entry| {
            let entry = entry.trim_start_matches(|c: char| c.is_whitespace() || "*&(".contains(c));
            let name: String = entry.chars().take_while(|&c| is_word_char(c)).collect();
            is_local_name(&name).then_some(name)
        })
        .collect()
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
    }
}

/// The opener of a percent literal at the start of `text`, on line `line`:
/// its length and the literal it opens, or `None` when `text` opens none.
fn percent_literal(text: &[char], line: usize) -> Option<(usize, Frame)> {
    let (kind, at) = match text.get(1)? {
        c @ ('q' | 'Q' | 'w' | 'W' | 'i' | 'I' | 'r' | 's' | 'x') => (Some(*c), 2),
        _ => (None, 1),
    };
    let open = *text.get(at)?;
    let close = match open {
        '(' => ')',
        '[' => ']',
        '{' => '}',
        '<' => '>',
        // A bare `%` before `=` or a space is the modulo operator.
        '=' if kind.is_none() => return None,
        c if c.is_ascii_punctuation() => c,
        _ => return None,
    };
    let interpolates = !matches!(kind, Some('q' | 'w' | 'i' | 's'));
    Some((
        at + 1,
        literal(open, close, interpolates, "percent literal", line),
    ))
}

/// The length of a `?x` character literal at the start of `text`, or
/// `None` when the `?` is not one (`a ? b : c`, `?ab`).
fn char_literal_len(text: &[char]) -> Option<usize> {
    let c = *text.get(1).filter(|c| !c.is_whitespace())?;
    let len = if c == '\\' { 3 } else { 2 };
    // A character literal followed straight by a word is no literal: `?ab`
    // is a method name, and `?"a"` a ternary before a string.
    if text.get(len).is_some_and(|&next| is_word_char(next)) {
        return None;
    }
    Some(len.min(text.len()))
}

/// The heredoc opened by the text after a `<<` on line `line`: the length
/// of that text and the heredoc, or `None` when the `<<` is a shift.
fn heredoc_opener(text: &[char], line: usize) -> Option<(usize, Heredoc)> {
    let indented = matches!(text.first(), Some('~' | '-'));
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
            line,
            guessed: false,
        },
    ))
}

/// Every statement among the code lines `lines` that opens with the word
/// `depends_on`, joined across its continuation lines.
fn depends_on_statements(lines: &[CodeLine]) -> Vec<String> {
    let mut found = Vec::new();
    // The statement the previous line left open, whether it is a
    // `depends_on`, and how many brackets it has left open.
    let mut open: Option<(bool, String, i32)> = None;
    for line in lines {
        if line.text.trim().is_empty() {
            continue;
        }
        let segments: Vec<&str> = line.text.split(';').collect();
        let last = segments.len() - 1;
        for (i, segment) in segments.into_iter().enumerate() {
            let opens = opens_with_depends_on(segment);
            let (is_depends_on, mut text, mut balance) = match open.take() {
                Some((is_depends_on, mut text, balance)) if !opens => {
                    text.push('\n');
                    (is_depends_on, text, balance)
                }
                Some((was_depends_on, text, _)) => {
                    if was_depends_on {
                        found.push(text);
                    }
                    (true, String::new(), 0)
                }
                None => (opens, String::new(), 0),
            };
            text.push_str(segment);
            balance += bracket_balance(segment);
            let tail = segment.trim_end();
            let continues = balance > 0
                || line.open
                || tail.ends_with([',', '(', '[', '{', '|', '.', '\\'])
                || tail.ends_with("&&")
                || tail.ends_with("=>");
            if i == last && continues {
                open = Some((is_depends_on, text, balance));
            } else if is_depends_on {
                found.push(text);
            }
        }
    }
    if let Some((true, text, _)) = open {
        found.push(text);
    }
    found
}

fn opens_with_depends_on(statement: &str) -> bool {
    statement
        .trim_start_matches(|c: char| c == '(' || c.is_whitespace())
        .strip_prefix("depends_on")
        .is_some_and(|rest| !rest.starts_with(is_word_char))
}

/// The count of `(`, `[` and `{` in `code` minus the count of their closers.
fn bracket_balance(code: &str) -> i32 {
    code.chars()
        .map(|c| match c {
            '(' | '[' | '{' => 1,
            ')' | ']' | '}' => -1,
            _ => 0,
        })
        .sum()
}

/// True when `args` names an OS as a whole word: the key `macos:`,
/// `maximum_macos:` or `linux:`, or the symbol `:macos` or `:linux`.
fn names_an_os(args: &str) -> bool {
    let bytes = args.as_bytes();
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut start = 0;
    while start < bytes.len() {
        if !is_word(bytes[start]) {
            start += 1;
            continue;
        }
        let mut end = start;
        while end < bytes.len() && is_word(bytes[end]) {
            end += 1;
        }
        let word = &args[start..end];
        let symbol = matches!(word, "macos" | "linux")
            && start >= 1
            && bytes[start - 1] == b':'
            && (start < 2 || bytes[start - 2] != b':');
        let key = matches!(word, "macos" | "maximum_macos" | "linux")
            && bytes.get(end) == Some(&b':')
            && bytes.get(end + 1) != Some(&b':');
        if symbol || key {
            return true;
        }
        start = end;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::{
        CodeLine, CustomBlockOsFacts, UnreadReason, Unreadable, code_lines, custom_block_os_facts,
    };

    fn facts(other_os_block: bool, os_dependency: bool) -> CustomBlockOsFacts {
        CustomBlockOsFacts {
            other_os_block,
            os_dependency,
            unread: None,
        }
    }

    /// The code of a line keeps everything outside its literals, writes
    /// each literal that closes on the line as `""`, leaves out the part of
    /// one that stays open, and stops at a `#` outside them.
    #[test]
    fn code_lines_blank_literals_and_cut_the_comment() {
        let cases = [
            (r#"url "x" # on_linux later"#, r#"url "" "#),
            ("caveats \"a # b\"", "caveats \"\""),
            ("x 'it''s'", "x \"\"\"\""),
            (r##"x "a\"#b" y"##, r#"x "" y"#),
            ("x %q(a(b)#c) y", "x \"\" y"),
            (
                "x %w[a b], %i{c}, %r<d>, %Q|e#|, %W!f!, %I/g/",
                "x \"\", \"\", \"\", \"\", \"\", \"\"",
            ),
            ("x %(a#b), %x(c), %s(d) y", "x \"\", \"\", \"\" y"),
            ("x = \"a\" %(b) # c", "x = \"\" %(b) "),
            ("a % b # c", "a % b "),
            ("n = a%(b) # c", "n = a%(b) "),
            ("x ?# y ?\" z ?' w", "x \"\" y \"\" z \"\" w"),
            ("empty?(x) # c", "empty?(x) "),
            ("a ? b : c # d", "a ? b : c "),
            ("x =~ /#/ # c", "x =~ \"\" "),
            ("y = a / b # c", "y = a / b "),
            ("f(/#/, /#/) # c", "f(\"\", \"\") "),
            ("\"a#{\"}\"}b\" # c", "\"\" "),
            ("\"a#{'\"'}b\" # c", "\"\" "),
            ("\"#{{a: 1}[:a]}\" x", "\"\" x"),
            ("caveats <<~EOS # c", "caveats \"\" "),
            ("m = 1<<FLAGS # c", "m = 1<<FLAGS "),
            ("m = a << 1 <<2 # c", "m = a << 1 <<2 "),
            ("@args <<extra # c", "@args <<extra "),
            ("$out <<x # c", "$out <<x "),
            ("self <<x # c", "self <<x "),
            ("end <<~EOS # c", "end <<~EOS "),
            ("nil ?\"a\" : 1 # c", "nil ?\"\" : 1 "),
            ("y = 1; x = y <<z # c", "y = 1; x = y <<z "),
            ("each { |a, b| a <<b } # c", "each { |a, b| a <<b } "),
            ("each do |x| x %-2 end # c", "each do |x| x %-2 end "),
            ("obj.y = 1; y <<~EOS", "obj.y = 1; y \"\""),
            ("z == 1; z <<~EOS", "z == 1; z \"\""),
        ];
        for (line, want) in cases {
            assert_eq!(
                code_lines(line).0,
                [CodeLine {
                    text: want.to_string(),
                    open: false
                }],
                "{line}"
            );
        }
    }

    /// A literal or heredoc that does not close on its line carries on into
    /// the following lines; its content is left out until it closes.
    #[test]
    fn an_open_literal_carries_into_the_next_lines() {
        let line = |text: &str, open: bool| CodeLine {
            text: text.to_string(),
            open,
        };
        let cases: &[(&str, &[CodeLine])] = &[
            (
                "x \"open # not a comment\non_linux\" y",
                &[line("x ", true), line("\"\" y", false)],
            ),
            (
                "puts %w[\n on_linux\n] y",
                &[line("puts ", true), line("", true), line("\"\" y", false)],
            ),
            (
                "\"#{\n x \"}\" \n}\" y",
                &[line("", true), line("", true), line("\"\" y", false)],
            ),
            (
                "caveats <<~EOS, <<-'B'\n on_linux\n  EOS\n  B\nz",
                &[
                    line("caveats \"\", \"\"", false),
                    line("", false),
                    line("", false),
                    line("", false),
                    line("z", false),
                ],
            ),
            (
                "puts <<EOS\n  EOS\neos\nEOS\nz",
                &[
                    line("puts \"\"", false),
                    line("", false),
                    line("", false),
                    line("", false),
                    line("z", false),
                ],
            ),
        ];
        for (block, want) in cases {
            assert_eq!(code_lines(block), (want.to_vec(), None), "{block}");
        }
    }

    /// What `custom_block_os_facts` reads from a `custom_block`, case by
    /// case. Both facts read the code of each line. A Linux side holds when
    /// its word appears there as a whole word. An OS dependency holds when
    /// a statement that opens with `depends_on`, read across its
    /// continuation lines, names an OS key or symbol. A wrong verdict
    /// renders a cask Homebrew refuses, so the over-match below is the case
    /// `depends_on_macos: true` exists for and the misses are the cases for
    /// `depends_on_macos: false`.
    #[test]
    fn custom_block_os_facts_reads_whole_words_in_the_code() {
        let cases: &[(&str, &str, CustomBlockOsFacts)] = &[
            ("empty", "", facts(false, false)),
            (
                "top-level on_linux",
                "on_linux do\n  binary \"x\"\nend",
                facts(true, false),
            ),
            (
                "top-level on_system",
                "on_system :linux do\nend",
                facts(true, false),
            ),
            ("app_image", "app_image \"X.AppImage\"", facts(true, false)),
            (
                "on_linux nested in on_macos",
                "on_macos do\n  on_linux do\n  end\nend",
                facts(true, false),
            ),
            (
                "depends_on macos: nested in on_macos",
                "on_macos do\n  depends_on macos: \">= :sonoma\"\nend",
                facts(false, true),
            ),
            (
                "depends_on after ; in a one-line block",
                "on_macos do; depends_on macos: :sonoma; end",
                facts(false, true),
            ),
            (
                "depends_on spread over several lines",
                "depends_on(\n  macos: \">= :big_sur\"\n)",
                facts(false, true),
            ),
            (
                "depends_on macos: with a symbol in its value",
                "depends_on macos: \">= :big_sur\"",
                facts(false, true),
            ),
            (
                "an OS name in a comment after a depends_on without one",
                "depends_on formula: \"jq\" # needs :macos 13",
                facts(false, false),
            ),
            (
                "an OS name in a comment on the next line",
                "depends_on formula: \"jq\"\n# needs :macos 13",
                facts(false, false),
            ),
            (
                "an OS name in the statement after the depends_on",
                "depends_on formula: \"jq\"; caveats \":macos\"",
                facts(false, false),
            ),
            (
                "a whole-line comment naming an OS dependency",
                "  # depends_on macos: \">= :ventura\"",
                facts(false, false),
            ),
            (
                "a whole-line comment naming a Linux side",
                "# on_linux do\n#   binary \"x\"\n# end",
                facts(false, false),
            ),
            (
                "on_linux in a comment after code",
                "url \"x\" # on_linux later",
                facts(false, false),
            ),
            (
                "a # inside a string cuts nothing",
                "caveats \"a # b\"\non_linux do\nend",
                facts(true, false),
            ),
            (
                "a # inside a %q literal cuts nothing",
                "depends_on formula: %q(a#b), macos: :ventura",
                facts(false, true),
            ),
            (
                "a # inside a %q literal with a symmetric delimiter",
                "depends_on formula: %q|a#b|, macos: :ventura",
                facts(false, true),
            ),
            (
                "depends_on inside a string",
                "caveats \"this depends_on macos: newer\"",
                facts(false, false),
            ),
            (
                "a ; inside a string starts no statement",
                "caveats \"a; depends_on macos: :ventura\"",
                facts(false, false),
            ),
            (
                "a ) inside a string does not close the call",
                "depends_on(formula: \"a)b\",\n  macos: \">= :ventura\")",
                facts(false, true),
            ),
            (
                "a trailing comma continues the statement",
                "depends_on arch: :arm64,\n  macos: \">= :ventura\"",
                facts(false, true),
            ),
            (
                "a comment line inside a continued statement",
                "depends_on arch: :arm64,\n  # newer systems only\n  macos: :ventura",
                facts(false, true),
            ),
            (
                "a blank line inside a continued statement",
                "depends_on arch: :arm64,\n\n  macos: :ventura",
                facts(false, true),
            ),
            (
                "a trailing backslash continues the statement",
                "depends_on arch: :arm64 \\\n  , macos: :ventura",
                facts(false, true),
            ),
            (
                "an open [ continues the statement until it closes",
                "depends_on cask: [\n  \"a\",\n], macos: :ventura",
                facts(false, true),
            ),
            (
                "an open { continues the statement until it closes",
                "depends_on({\n  formula: \"a\"\n}.merge(macos: :ventura))",
                facts(false, true),
            ),
            (
                "a closed list ends the statement",
                "depends_on cask: [\"a\"]\nmacos: :ventura",
                facts(false, false),
            ),
            (
                "a trailing comment is cut before the comma is read",
                "depends_on arch: :arm64, # :macos below\n  formula: \"x\"",
                facts(false, false),
            ),
            (
                "depends_on as the last token of a line",
                "x = depends_on\nmacos: :ventura",
                facts(false, false),
            ),
            (
                "a bare depends_on ends its statement",
                "depends_on\nmacos: :ventura",
                facts(false, false),
            ),
            (
                "a second ;-statement opens with depends_on",
                "depends_on formula: \"x\"; depends_on macos: :ventura",
                facts(false, true),
            ),
            (
                "a depends_on line inside an open { starts its own statement",
                "foo = {\n  depends_on macos: :ventura\n}",
                facts(false, true),
            ),
            (
                "an OS key on a later line of a depends_on( call",
                "depends_on(\n  formula: \"jq\",\n  linux: :any\n)",
                facts(false, true),
            ),
            (
                "a constant path is not a symbol",
                "depends_on formula: Foo::macos",
                facts(false, false),
            ),
            (
                "maximum_macos: contains macos:",
                "depends_on maximum_macos: :sonoma",
                facts(false, true),
            ),
            ("bare :macos", "depends_on :macos", facts(false, true)),
            ("linux: hash", "depends_on linux: :any", facts(false, true)),
            (
                "a depends_on without an OS",
                "depends_on formula: \"x\"",
                facts(false, false),
            ),
            (
                "an OS name without a depends_on",
                "caveats \":macos\"",
                facts(false, false),
            ),
            (
                "an OS name inside a string of a depends_on",
                "depends_on cask: \":macos\"",
                facts(false, false),
            ),
            (
                "on_linux in a string and in a comment",
                "caveats \"on_linux\"\n# on_linux",
                facts(false, false),
            ),
            (
                "an apostrophe in a %q string, then on_linux",
                "caveats %q(Don't forget)\non_linux do\nend",
                facts(true, false),
            ),
            (
                "a << shift, then on_linux",
                "m = 1<<FLAGS\non_linux do\nend",
                facts(true, false),
            ),
            (
                "a << after a call is a shift, then on_linux",
                "x = foo() <<x\non_linux do\nend",
                facts(true, false),
            ),
            (
                "a << after an index is a shift, then on_linux",
                "x = list[0] <<y\non_linux do\nend",
                facts(true, false),
            ),
            (
                "class <<self is a shift, then on_linux",
                "class <<self\nend\non_linux do\nend",
                facts(true, false),
            ),
            (
                "a ? after a call is a ternary, then on_linux",
                "x = foo() ?\"a\" : \"b\"\non_linux do\nend",
                facts(true, false),
            ),
            (
                "a ? before a quote and a word is a ternary, then on_linux",
                "x = c ?\"a\" : \"b\"\non_linux do\nend",
                facts(true, false),
            ),
            (
                "a % after a parenthesis is a modulo, then on_linux",
                "n = (a) %-2\non_linux do\nend",
                facts(true, false),
            ),
            (
                "a << after a local is a shift, then on_linux",
                "y = 1\nx = y <<z\non_linux do\nend",
                facts(true, false),
            ),
            (
                "a << after an instance variable is a shift, then on_linux",
                "@args <<extra\non_linux do\nend",
                facts(true, false),
            ),
            (
                "a << after a global variable is a shift, then on_linux",
                "$stdout <<extra\non_linux do\nend",
                facts(true, false),
            ),
            (
                "a << after self is a shift, then on_linux",
                "self <<x\non_linux do\nend",
                facts(true, false),
            ),
            (
                "a << after end is a shift, then on_linux",
                "end <<~EOS\non_linux do\nend",
                facts(true, false),
            ),
            (
                "a % after a local is a modulo, then on_linux",
                "m = 7\nn = m %-2\non_linux do\nend",
                facts(true, false),
            ),
            (
                "a ? after a local before an empty string is a ternary, then on_linux",
                "cond = true\nx = cond ?\"\" : \"no\"\non_linux do\nend",
                facts(true, false),
            ),
            (
                "a ? after a local before a quoted space is a ternary, then on_linux",
                "cond = true\nx = cond ?' ' : ''\non_linux do\nend",
                facts(true, false),
            ),
            (
                "a ? after a block parameter is a ternary, then on_linux",
                "[1].each do |cond|\n  x = cond ?\"\" : \"no\"\nend\non_linux do\nend",
                facts(true, false),
            ),
            (
                "a << after a brace block parameter is a shift, then on_linux",
                "list.map { |a, b| a <<b }\non_linux do\nend",
                facts(true, false),
            ),
            (
                "a << after nil, true or false is a shift, then on_linux",
                "nil <<a; true <<b; false <<c\non_linux do\nend",
                facts(true, false),
            ),
            (
                "an == comparison assigns no local, so its percent literal hides on_linux",
                "z == 1\nz %(on_linux)",
                facts(false, false),
            ),
            (
                "an attribute assignment assigns no local, so its percent literal hides on_linux",
                "obj.y = 1\ny %(on_linux)",
                facts(false, false),
            ),
            (
                "a heredoc after a method name hides on_linux",
                "caveats <<~EOS\n  on_linux\nEOS\n",
                facts(false, false),
            ),
            (
                "two heredocs as arguments hide on_linux",
                "puts(<<~A, <<~B)\n on_linux\nA\n on_linux\nB\n",
                facts(false, false),
            ),
            (
                "a quoted heredoc tag with a dash, then on_linux",
                "caveats <<~'END-TEXT'\n  text\nEND-TEXT\non_linux do\nend",
                facts(true, false),
            ),
            (
                "on_linux after a string with a ; inside it",
                "caveats \"a; on_linux\"; on_linux do\nend",
                facts(true, false),
            ),
            (
                "on_linux behind a receiver",
                "self.on_linux do\nend",
                facts(true, false),
            ),
            (
                "on_linux inside parentheses",
                "(on_linux do\nend)",
                facts(true, false),
            ),
            (
                "on_linux nested on one line",
                "on_arm do on_linux do binary \"x\" end end",
                facts(true, false),
            ),
            (
                "on_linux inside a %w[] list over several lines",
                "%w[\n on_linux\n]",
                facts(false, false),
            ),
            (
                "on_linux on a heredoc body line",
                "caveats <<~EOS\n  on_linux\nEOS\n",
                facts(false, false),
            ),
            (
                "a heredoc body, then a depends_on",
                "caveats <<~EOS\non_linux\nEOS\ndepends_on macos: :big_sur",
                facts(false, true),
            ),
            (
                "a string over two lines, then a depends_on",
                "caveats \"a\nb\"; depends_on macos: :ventura",
                facts(false, true),
            ),
            (
                "a string over two lines, then on_linux",
                "caveats \"a\nb\"; on_linux do\nend",
                facts(true, false),
            ),
            (
                "on_linux in a string inside an interpolation",
                "\"#{\"on_linux\"}\"",
                facts(false, false),
            ),
            (
                "a quote inside an interpolation does not close the string",
                "\"a#{'\"'}b\"; on_linux do\nend",
                facts(true, false),
            ),
            (
                "on_linux in a bare %() literal",
                "caveats %(on_linux)",
                facts(false, false),
            ),
            (
                "a # inside a bare %() literal cuts nothing",
                "depends_on formula: %(a#b), macos: :ventura",
                facts(false, true),
            ),
            (
                "a # in a regex after =~ cuts nothing",
                "x =~ /#/; on_linux do\nend",
                facts(true, false),
            ),
            (
                "a # in a char literal cuts nothing",
                "x = ?#; on_linux do\nend",
                facts(true, false),
            ),
            (
                "a trailing => continues the statement",
                "depends_on :formula =>\n \"x\", :macos => :ventura",
                facts(false, true),
            ),
            (
                "a depends_on statement inside parentheses",
                "(depends_on macos: :ventura)",
                facts(false, true),
            ),
            (
                "a string spanning lines inside a depends_on",
                "depends_on formula: \"a\nb\", macos: :ventura",
                facts(false, true),
            ),
            (
                "a string spanning lines holding an OS key",
                "depends_on formula: \"a\nmacos: b\"",
                facts(false, false),
            ),
            (
                "a longer word holding on_linux or depends_on",
                "end_foo\non_linux_helper\nmy_on_system\ndepends_on_x :macos\n",
                facts(false, false),
            ),
            // An over-match: a regex after a token not in the list is read
            // as code, which `depends_on_macos: true` exists for.
            (
                "on_linux in a regex after when",
                "case x\nwhen /on_linux/ then 1\nend",
                facts(true, false),
            ),
            // Misses: a Linux side built at runtime, which `depends_on_macos:
            // false` exists for.
            (
                "an on_linux built at runtime",
                "send(:\"on_#{\"lin\"}ux\") do\n  url \"https://e.com/l.tar.gz\"\nend\n",
                facts(false, false),
            ),
            (
                "a depends_on after a do on the same line",
                "on_macos do depends_on macos: :ventura end",
                facts(false, false),
            ),
            (
                "an on_linux defined at runtime",
                "define_method(:x) { send(\"on_\" + \"linux\") { } }",
                facts(false, false),
            ),
        ];
        for (label, block, want) in cases {
            assert_eq!(
                custom_block_os_facts(Some(block)),
                *want,
                "{label}:\n{block}"
            );
        }
        assert_eq!(custom_block_os_facts(None), facts(false, false));
    }

    fn unclosed(line: usize, kind: &'static str) -> Option<Unreadable> {
        Some(Unreadable {
            line,
            kind,
            reason: UnreadReason::Unclosed,
        })
    }

    fn ambiguous(line: usize, kind: &'static str, operator: &'static str) -> Option<Unreadable> {
        Some(Unreadable {
            line,
            kind,
            reason: UnreadReason::Ambiguous { operator },
        })
    }

    /// The facts for `block`, asserted against the verdict and the place,
    /// if any, the block could not be read past.
    fn assert_facts(
        block: &str,
        other_os_block: bool,
        os_dependency: bool,
        unread: Option<Unreadable>,
    ) {
        assert_eq!(
            custom_block_os_facts(Some(block)),
            CustomBlockOsFacts {
                other_os_block,
                os_dependency,
                unread,
            },
            "{block}"
        );
    }

    /// A literal or heredoc still open after the last line means the block
    /// was misread. Both facts then come from a word scan of the raw text
    /// with its comments cut, and `unread` names the line the open literal
    /// starts on and its kind.
    #[test]
    fn an_unclosed_literal_falls_back_to_the_word_scan() {
        let cases: &[(&str, &str, CustomBlockOsFacts)] = &[
            (
                "an unclosed string hides nothing after it",
                "url \"x\"\ncaveats \"never closed\non_linux do\nend",
                CustomBlockOsFacts {
                    other_os_block: true,
                    os_dependency: false,
                    unread: unclosed(2, "string"),
                },
            ),
            (
                "a ? after a name that is no local reads a character literal, and the string after it never closes",
                "x = cond ?\"\" : \"no\"\ndepends_on macos: :ventura",
                CustomBlockOsFacts {
                    other_os_block: false,
                    os_dependency: true,
                    unread: ambiguous(1, "character literal", "?"),
                },
            ),
            (
                "a heredoc with no closing tag",
                "caveats <<~EOS\n  text\non_linux do\nend",
                CustomBlockOsFacts {
                    other_os_block: true,
                    os_dependency: false,
                    unread: unclosed(1, "heredoc"),
                },
            ),
            (
                "an unclosed percent literal; the word scan cuts a comment",
                "x = %w[a\n# on_linux\n",
                CustomBlockOsFacts {
                    other_os_block: false,
                    os_dependency: false,
                    unread: unclosed(1, "percent literal"),
                },
            ),
            (
                "an unclosed regex; an OS word with no depends_on is no dependency",
                "x =~ /a\n:macos",
                CustomBlockOsFacts {
                    other_os_block: false,
                    os_dependency: false,
                    unread: unclosed(1, "regex"),
                },
            ),
        ];
        for (label, block, want) in cases {
            assert_eq!(
                custom_block_os_facts(Some(block)),
                *want,
                "{label}:\n{block}"
            );
        }
    }

    /// A comment after an unclosed heredoc names no OS for the word scan: a
    /// `#` that starts a line or follows whitespace cuts the line, and a `#`
    /// inside a word does not.
    #[test]
    fn the_word_scan_cuts_comments() {
        assert_facts(
            "opts ||= []\nopts <<\"--quiet\"\ndepends_on arch: :arm64\n# built for macos: 13 and newer\ncaveats <<~EOS\n",
            false,
            false,
            unclosed(5, "heredoc"),
        );
        assert_facts(
            "caveats <<~EOS\ndepends_on formula: \"a\" # macos: 13\n",
            false,
            false,
            unclosed(1, "heredoc"),
        );
        assert_facts(
            "caveats <<~EOS\ndepends_on x#macos: :ventura\n",
            false,
            true,
            unclosed(1, "heredoc"),
        );
    }

    /// An untracked word before a `<<`, `%` or `?` may be a local the lexer
    /// did not see: the operator is read as an opener, and when the literal
    /// it opens spans more than one line the block falls back to the word
    /// scan. One that closes on its line stays read as code.
    #[test]
    fn an_ambiguous_opener_spanning_lines_falls_back_to_the_word_scan() {
        assert_facts(
            "x = width %-2\non_linux do\nend\nn = 3 - 1",
            true,
            false,
            ambiguous(1, "percent literal", "%"),
        );
        assert_facts("y = width %(a) + 1\non_linux do\nend", true, false, None);
        assert_facts(
            "z <<~EOS\n  on_linux\nEOS\n",
            true,
            false,
            ambiguous(1, "heredoc", "<<"),
        );
        assert_facts("caveats <<~EOS\n  on_linux\nEOS\n", false, false, None);
    }

    /// `a, b = 10, 3` binds both names, so `a %-b` is a modulo.
    #[test]
    fn a_multiple_assignment_binds_every_name() {
        assert_facts(
            "a, b = 10, 3\nr = a %-b\ndepends_on macos: \">= :ventura\"\npad = 1 - 2",
            false,
            true,
            None,
        );
    }

    /// `flag ||= true` binds `flag`, so `flag ?'-'` is a ternary.
    #[test]
    fn an_operator_assignment_binds_its_name() {
        assert_facts(
            "flag ||= true\nsep = flag ?'-' : '_'\non_linux do\n  binary \"tool\"\nend\n# it's built per arch",
            true,
            false,
            None,
        );
        for operator in ["&&=", "+=", "-=", "*=", "/="] {
            assert_facts(
                &format!("n {operator} 1\nm = n %-2\npad = 3 - 1\non_linux do\nend"),
                true,
                false,
                None,
            );
        }
    }

    /// `for n in [1, 2]` binds `n`, so `n %-2` is a modulo.
    #[test]
    fn a_for_loop_binds_its_variable() {
        assert_facts(
            "for n in [1, 2]\n  m = n %-2\nend\ndepends_on macos: :ventura\npad = 3 - 1",
            false,
            true,
            None,
        );
    }

    /// Every parameter of a `def`, with or without parentheses, defaults,
    /// `*rest`, `**kw` and `&blk`, is read as a value.
    #[test]
    fn a_method_definition_binds_its_parameters() {
        assert_facts(
            "def self.wrap(a, b = 1)\n  a %-b\nend\non_linux do\n  binary \"tool\"\nend\npad = 3 - 1",
            true,
            false,
            None,
        );
        for name in ["a", "b", "rest", "kw", "blk", "k"] {
            for head in [
                "def wrap(a, b = [1, 2], *rest, k: 1, **kw, &blk)",
                "def wrap a, b = 1, *rest, k: 1, **kw, &blk",
            ] {
                assert_facts(
                    &format!("{head}\n  {name} %-2\nend\npad = 3 - 1\non_linux do\nend"),
                    true,
                    false,
                    None,
                );
            }
        }
    }

    /// `->(x)` and `-> (x)` bind `x` for the lambda body on the same line.
    #[test]
    fn a_lambda_binds_its_parameters() {
        for head in ["->(x)", "-> (x)"] {
            assert_facts(
                &format!("half = {head} {{ x %-2 }}\ndepends_on macos: :ventura\npad = 3 - 1"),
                false,
                true,
                None,
            );
        }
    }

    /// `rescue => e` binds `e`, so `e %-1` is a modulo.
    #[test]
    fn a_rescue_binds_its_variable() {
        assert_facts(
            "begin\n  Integer(\"x\")\nrescue => e\n  e %-1 rescue nil\nend\non_system :linux, macos: :ventura do\n  binary \"tool\"\nend\npad = 3 - 1",
            true,
            false,
            None,
        );
    }

    /// `(lo, hi) = 1, 9` binds both names, so `lo %-hi` is a modulo.
    #[test]
    fn a_parenthesized_multiple_assignment_binds_every_name() {
        assert_facts(
            "(lo, hi) = 1, 9\nr = lo %-hi\ndepends_on macos: :ventura\npad = 3 - 1",
            false,
            true,
            None,
        );
    }

    /// A block parameter list spread over several lines binds every name.
    #[test]
    fn a_block_parameter_list_over_several_lines_binds_every_name() {
        assert_facts(
            "v = [[1, 2]].map do |a,\n  b|\n  a %-b\nend\non_linux do\n  binary \"tool\"\nend\npad = 3 - 1",
            true,
            false,
            None,
        );
    }

    /// A block parameter is no local after its block, so a `<<` after its
    /// name whose heredoc tag closes on a later line is ambiguous, and the
    /// block falls back to the word scan with a warning. A `<<` whose tag
    /// never closes can only be a shift.
    #[test]
    fn a_heredoc_after_a_block_parameter_falls_back_to_the_word_scan() {
        assert_facts(
            "[1].each do |caveats| end\ncaveats <<~EOS\n  Runs on_linux too.\nEOS",
            true,
            false,
            ambiguous(2, "heredoc", "<<"),
        );
        assert_facts(
            "list.map { |a, b| a <<b }\non_linux do\nend",
            true,
            false,
            None,
        );
    }

    /// Each line ending continues a `depends_on` statement onto the next
    /// line, so an OS key there is read as part of it.
    #[test]
    fn every_continuation_ending_joins_the_next_line() {
        for ending in [",", "(", "[", "{", "|", ".", "&&", "||", "=>", "\\"] {
            let block = format!("depends_on formula: \"x\" {ending}\n  macos: :ventura");
            assert_eq!(
                custom_block_os_facts(Some(&block)),
                facts(false, true),
                "{ending}"
            );
        }
    }
}
