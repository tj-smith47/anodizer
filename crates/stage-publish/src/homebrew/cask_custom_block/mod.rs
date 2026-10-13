//! What a cask's `custom_block` says about its Linux side and its OS
//! dependencies, the two facts that decide whether the rendered cask
//! carries `depends_on :macos`.

use anodizer_core::log::StageLogger;

use super::cask::CaskParams;

mod heredoc;
mod lexer;
mod names;
mod words;

use lexer::{CodeLine, code_lines, lex_block};

#[cfg(test)]
mod tests;

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
    /// `regex`, `character literal` or `comment` (an `=begin` with no
    /// `=end`).
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
/// - a `"…"`, `'…'` or `` `…` `` string; inside a `"…"` or `` `…` ``
///   string, `#{…}` is read as code with its own braces and strings, so the
///   string closes at its own quote, and `#$"` interpolates the global `$"`;
/// - a heredoc body, opened by `<<~TAG`, `<<-TAG` or `<<TAG` with the tag
///   bare or quoted and closed by a line holding the tag alone (indented
///   for `<<~` and `<<-`), case-sensitive; unless the tag is
///   single-quoted, a body line ending in an odd run of `\` joins the next
///   line, so a tag on that line does not close the heredoc, and `#{…}` in
///   a body line is read as code: a heredoc opened there has its body read
///   from the next line, before the rest of the body around it, and a body
///   line that ends inside `#{…}` cannot be read past; `<<` followed by a
///   space or a digit is a shift;
/// - a `%(…)`, `%q`, `%Q`, `%w`, `%W`, `%i`, `%I`, `%r`, `%s` or `%x`
///   literal delimited by `()`, `[]`, `{}` or `<>` (which nest) or by one
///   repeated punctuation character; a typed one also by a space or the end
///   of the line, except a list (`%w`, `%W`, `%i`, `%I`), whose words
///   whitespace separates, so whitespace never closes it; and a bare `%`
///   with nothing before it on its statement but an operator also by a
///   space, a tab, `=` or the end of the line. A literal delimited by the
///   end of its line holds the next line, and the one after while the held
///   line ends in an odd run of `\`, and its statement reads on past it; a
///   `%r` closed that way takes the letters that start the line after as
///   its options (`%r`, `ab`, `ix`), so that line begins inside it;
/// - a `?x` character literal (`?#`, `?"`, `?'`) unless a word character
///   follows a word character (`?ab`) or a quote (`?"a"`);
/// - a `/…/` regex where an operand stands;
/// - a `#` comment, which cuts the rest of the line, and an `=begin` …
///   `=end` comment.
///
/// None of these opens after a `:` that makes a symbol (`:%`, `` :` ``), in
/// a special global (`$"`, `$/`), or where it names a method: after `.`,
/// `&.` or `::`, with or without a space, whatever the name looks like
/// (`x.<<`, `x. %`, `a&.!~`), after `def` (`def /(other)`), and as the `?`
/// or `!` of a method name (`then?"a"`). Written straight after a method
/// name spelled as an operator, a `<<`, `%` or `/` is an operator and a `?`
/// opens a character literal.
///
/// A `<<`, `%`, `?` or `/` that follows a value is a shift, a modulo, a
/// ternary or a division and opens nothing. The code before it ends with a
/// value when, whitespace aside, it ends with `)`, `]`, `}`, a number or a
/// closed literal, or with one of these words:
///
/// - `self`, `nil`, `true`, `false`, `end`, `class` (`class <<self`) or
///   `def` (`def /(other)`);
/// - an instance, class or global variable (`@args`, `@@all`, `$stdout`);
/// - a local variable, which is a name assigned earlier in its scope:
///   `name =`, `name ||=` and the other operator assignments (`==`, `=~`,
///   `=>`, `!=`, `<=` and `>=` assign nothing, and neither does
///   `obj.name =`), `a, b =`, `a, (b, c) =`, `(a, b) =`, `a, =`,
///   `for a in`, `rescue => a`, a named group before `=~`
///   (`/(?<a>\d+)/ =~ s`), a name in an `in` pattern (`in [a, 2]`,
///   `x in {k: a}`) and the target of a rightward `=>` (`2 => a`);
/// - a parameter: of a block (`do |a, b; c|`, `{ |a| … }`, a list spread
///   over several lines), of a method (`def name(a, b = 1, *rest, **kw,
///   &blk)`, with or without parentheses; a setter `def name=(v)` binds `v`
///   alone) or of a lambda (`->(x)`, `-> (x)`);
/// - any word other than a keyword written straight against the operator
///   (`a%(b)`, `1<<FLAGS`); `if%w[a]` opens a literal.
///
/// A `def` body sees none of the locals outside it. A block, the body of a
/// lambda (`-> { … }`, `->(x) do … end`) included, sees them, and its
/// parameters and the locals first assigned in it end at its `end` or `}`.
/// `if`, `while`, `case` and `begin` open no scope. After a word, a `/`
/// followed by a space or `=` divides. A statement continued with a `\` at
/// the end of a line, with a `?\` character literal that holds the
/// newline, with a `.`, `&.` or `::` that ends a line (past blank and
/// comment lines; a `..` range continues nothing) or with a literal the
/// newline closes reads on as one statement: the code before the break
/// decides what a `%`, `<<`, `?`, `/`, `if` or `until` after it is.
///
/// The operator opens a literal after an operator, `(`, `,`, `;`, the start
/// of a line, a Ruby keyword, any method of the cask DSL (`caveats <<~EOS`,
/// `binary %w[a]`) or a common method that takes an argument
/// (`puts %w[a]`). After any other word followed by a space the lexer
/// cannot tell a method from a local it did not see assigned, and reads
/// the operator as an opener; that is a guess.
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
/// still open after the last line, when a guess there opens a literal that
/// spans several lines, or when a parameter still in scope is followed
/// there by a `<<` whose heredoc tag closes on a later line. Both facts
/// then come from a word scan of the raw text, comments and strings
/// included, which may over-match: a Linux side when one of its words
/// appears, an OS dependency when `depends_on` and an OS key or symbol both
/// do. [`CustomBlockOsFacts::unread`] names the line, for the warning
/// [`warn_unread_custom_block`] prints.
///
/// These are read wrongly with no warning: a stanza built at runtime
/// (through `send` or `define_method`); a `depends_on` after a `do` on the
/// same line; and a guess whose literal closes on the same line, whose
/// content is set aside. `depends_on_macos:` overrides the verdict for
/// every case.
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
pub(super) fn custom_block_os_facts(block: Option<&str>) -> CustomBlockOsFacts {
    let Some(block) = block else {
        return CustomBlockOsFacts::default();
    };
    let (code, unread) = code_lines(block);
    if let Some(unread) = unread {
        return CustomBlockOsFacts {
            other_os_block: names_a_linux_side(block),
            os_dependency: block
                .split(|c: char| !is_word_char(c))
                .any(|w| w == "depends_on")
                && names_an_os(block),
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

/// How one line of a `custom_block` may be laid out again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct LineEdges {
    /// The line begins in code, so it may be re-indented without changing
    /// what it means. A line that begins inside a string, percent literal,
    /// regex, heredoc body or `=begin` comment, and the closing tag of a
    /// `<<` or `<<-` heredoc, does not.
    pub(super) starts_in_code: bool,
    /// The line ends in code, and whitespace at or past this byte offset
    /// is trailing and may be dropped. The offset is past the last literal
    /// on the line, so a delimiter or content that is whitespace (`?\ `,
    /// `% a `) is kept. `None` for a line that ends inside a literal,
    /// heredoc body or `=begin` comment, which is kept as written.
    pub(super) trim_from: Option<usize>,
}

/// How each line of `block` may be laid out again. Every line after the
/// place the block cannot be read past is kept as written.
pub(super) fn line_code_edges(block: &str) -> Vec<LineEdges> {
    let lexed = lex_block(block);
    let readable = lexed.unread.map_or(usize::MAX, |u| u.line);
    lexed
        .starts
        .into_iter()
        .zip(lexed.code_ends)
        .enumerate()
        .map(|(n, (starts, end))| LineEdges {
            starts_in_code: starts && n < readable,
            trim_from: end.filter(|_| n < readable),
        })
        .collect()
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

/// True for a character Ruby allows in a name: an ASCII letter, digit or
/// `_`, or any character outside ASCII.
fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || !c.is_ascii()
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
    name.starts_with(|c: char| {
        c.is_ascii_lowercase() || c == '_' || (!c.is_ascii() && !c.is_uppercase())
    })
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
