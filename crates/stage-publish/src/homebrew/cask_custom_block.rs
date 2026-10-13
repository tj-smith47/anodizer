//! What a cask's `custom_block` says about its Linux side and its OS
//! dependencies, the two facts that decide whether the rendered cask
//! carries `depends_on :macos`.

/// What a `custom_block` contributes to the `depends_on :macos` decision.
#[derive(Default, Debug, PartialEq, Eq)]
pub(super) struct CustomBlockOsFacts {
    /// The word `on_linux`, `on_system` or the Linux-only `app_image`
    /// appears in the code of the block: the cask has a Linux side.
    pub(super) other_os_block: bool,
    /// A `depends_on` statement names an OS: the key `macos:`,
    /// `maximum_macos:` or `linux:`, or the symbol `:macos` or `:linux`.
    pub(super) os_dependency: bool,
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
/// literal, with the word `class` (`class <<self`), or with a word written
/// straight against the operator (`a%(b)`, `1<<FLAGS`). It does not after
/// an operator, `(`, `,`, `;`, the start of a line, or a word followed by a
/// space, which is a method or keyword taking an argument
/// (`caveats <<~EOS`, `puts %w[a]`).
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
// and fails to load on Linux. A stanza built at runtime (through `send` or
// `define_method`), or a `depends_on` after a `do` on the same line, is a
// miss, and a regex after any other token is read as code.
// `depends_on_macos:` overrides the verdict for every case.
pub(super) fn custom_block_os_facts(block: Option<&str>) -> CustomBlockOsFacts {
    let Some(block) = block else {
        return CustomBlockOsFacts::default();
    };
    let code = code_lines(block);
    let other_os_block = code
        .iter()
        .flat_map(|line| line.text.split(|c: char| !is_word_char(c)))
        .any(|w| matches!(w, "on_linux" | "on_system" | "app_image"));
    let os_dependency = depends_on_statements(&code)
        .iter()
        .any(|statement| names_an_os(statement));
    CustomBlockOsFacts {
        other_os_block,
        os_dependency,
    }
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// The code of one line of a `custom_block`.
#[derive(Debug, Default, PartialEq, Eq)]
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
    },
    /// `#{…}` code, ended by the `}` that matches no `{` opened inside it.
    Interpolation { braces: usize },
}

/// A heredoc whose body begins on the line after its opener.
struct Heredoc {
    tag: String,
    indented: bool,
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

/// The code of every line of `block`. Only the open literals and the
/// pending heredocs carry from one line to the next.
fn code_lines(block: &str) -> Vec<CodeLine> {
    let mut stack = Vec::new();
    let mut heredocs: Vec<Heredoc> = Vec::new();
    let mut out = Vec::new();
    for line in block.lines() {
        if let Some(heredoc) = heredocs.first() {
            if heredoc.closes_on(line) {
                heredocs.remove(0);
            }
            out.push(CodeLine::default());
            continue;
        }
        out.push(lex_line(line, &mut stack, &mut heredocs));
    }
    out
}

/// Read one line on top of the literals `stack` left open, queueing the
/// heredocs it opens in `heredocs`.
fn lex_line(line: &str, stack: &mut Vec<Frame>, heredocs: &mut Vec<Heredoc>) -> CodeLine {
    let chars: Vec<char> = line.chars().collect();
    let mut text = String::new();
    // The last two characters of code read, ignoring whitespace; `None`
    // at the start of a statement.
    let mut prev: Option<char> = None;
    let mut before_prev: Option<char> = None;
    // The word or number the code read so far ends with, empty when it ends
    // with anything else, and whether whitespace followed it.
    let mut word = String::new();
    let mut spaced = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if let Some(Frame::Literal {
            open,
            close,
            depth,
            interpolates,
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
        let after_value = after_value(prev, &word, spaced);
        let rest = &chars[i..];
        let opened = match c {
            '#' => break,
            '"' | '\'' => Some((1, Some(literal(c, c, c == '"')))),
            '%' if !after_value => percent_literal(rest).map(|(len, frame)| (len, Some(frame))),
            '?' if !after_value => char_literal_len(rest).map(|len| (len, None)),
            '/' if prev.is_none()
                || matches!(prev, Some('(' | ',' | '='))
                || (prev == Some('~') && matches!(before_prev, Some('=' | '!'))) =>
            {
                Some((1, Some(literal('/', '/', true))))
            }
            '<' if !after_value && rest.get(1) == Some(&'<') => {
                heredoc_opener(&rest[2..]).map(|(len, heredoc)| {
                    heredocs.push(heredoc);
                    (2 + len, None)
                })
            }
            _ => None,
        };
        if let Some((len, frame)) = opened {
            i += len;
            match frame {
                Some(frame) => stack.push(frame),
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
        }
        if c.is_whitespace() {
            spaced = true;
        } else {
            if !is_word_char(c) || spaced || !prev.is_some_and(is_word_char) {
                word.clear();
            }
            if is_word_char(c) {
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
    CodeLine {
        text,
        open: !stack.is_empty(),
    }
}

/// True when the code before a `<<`, `?` or `%` ends with a value, which
/// makes it a shift, a ternary or a modulo. `prev` is the last code
/// character (`"` for a literal that closed), `word` the word or number the
/// code ends with, and `spaced` whether whitespace followed it.
///
/// A value ends with `)`, `]`, `}`, a closed literal or a number, or is a
/// word written straight against the operator (`a%(b)`). The word `class`
/// counts too, for `class <<self`. Any other word is a method or keyword
/// taking an argument (`caveats <<~EOS`, `puts %w[a]`), and an operator,
/// `(`, `,` or the start of a statement leaves no value.
fn after_value(prev: Option<char>, word: &str, spaced: bool) -> bool {
    match prev {
        Some(')' | ']' | '}' | '"') => true,
        Some(c) if is_word_char(c) => {
            !spaced || word == "class" || word.starts_with(|c: char| c.is_ascii_digit())
        }
        _ => false,
    }
}

fn literal(open: char, close: char, interpolates: bool) -> Frame {
    Frame::Literal {
        open,
        close,
        depth: 0,
        interpolates,
    }
}

/// The opener of a percent literal at the start of `text`: its length and
/// the literal it opens, or `None` when `text` opens none.
fn percent_literal(text: &[char]) -> Option<(usize, Frame)> {
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
    Some((at + 1, literal(open, close, interpolates)))
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

/// The heredoc opened by the text after a `<<`: the length of that text
/// and the heredoc, or `None` when the `<<` is a shift.
fn heredoc_opener(text: &[char]) -> Option<(usize, Heredoc)> {
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
    use super::{CodeLine, CustomBlockOsFacts, code_lines, custom_block_os_facts};

    fn facts(other_os_block: bool, os_dependency: bool) -> CustomBlockOsFacts {
        CustomBlockOsFacts {
            other_os_block,
            os_dependency,
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
        ];
        for (line, want) in cases {
            assert_eq!(
                code_lines(line),
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
                "x %w[\n on_linux\n] y",
                &[line("x ", true), line("", true), line("\"\" y", false)],
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
                "x <<EOS\n  EOS\neos\nEOS\nz",
                &[
                    line("x \"\"", false),
                    line("", false),
                    line("", false),
                    line("", false),
                    line("z", false),
                ],
            ),
        ];
        for (block, want) in cases {
            assert_eq!(code_lines(block), *want, "{block}");
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
