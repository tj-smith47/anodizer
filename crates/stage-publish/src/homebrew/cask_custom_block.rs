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
/// Each line is first reduced to its code, and both facts read only that:
///
/// - The contents of a `"…"` or `'…'` string, and of a `%q`, `%Q`, `%w`,
///   `%W`, `%i`, `%I` or `%r` literal delimited by `()`, `[]`, `{}`, `<>`
///   or one repeated punctuation character, are left out when the literal
///   closes on the same line.
/// - A `#` outside those literals cuts the rest of the line, so a comment
///   line reads as blank.
///
/// Then:
///
/// - A Linux side holds when `on_linux`, `on_system` or `app_image` appears
///   as a whole word, so a receiver (`self.on_linux do`), a parenthesis or
///   a one-line nesting still counts.
/// - A `depends_on` statement starts where `depends_on` is the first word
///   of a line or of a `;`-separated statement. It continues onto the next
///   line while the current one ends with `,`, `(`, `[`, `{`, `|`, `.`,
///   `&&`, `||` or `\`, or while the statement has opened more `(`, `[`
///   and `{` than it has closed. A blank line inside it is passed over, and
///   a line that opens with `depends_on` starts a new statement.
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
// and fails to load on Linux. Heredoc bodies and literals that span lines
// are read as code, so a stanza word inside one is an over-match; a stanza
// built at runtime, or a `depends_on` after a `do` on the same line, is a
// miss. `depends_on_macos:` overrides the verdict for every case.
pub(super) fn custom_block_os_facts(block: Option<&str>) -> CustomBlockOsFacts {
    let Some(block) = block else {
        return CustomBlockOsFacts::default();
    };
    let code: Vec<String> = block.lines().map(code_text).collect();
    let other_os_block = code
        .iter()
        .flat_map(|line| line.split(|c: char| !is_word_char(c)))
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

/// The code of one line: every string and percent literal that closes on
/// the line written as an empty `""`, and the line cut at the first `#`
/// outside them. A quote left open runs to the end of the line, and that
/// rest is dropped.
fn code_text(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut at = 0;
    while let Some(c) = line[at..].chars().next() {
        match c {
            '#' => break,
            '"' | '\'' => {
                let Some(len) = literal_body_len(&line[at + 1..], c, c) else {
                    break;
                };
                out.push_str("\"\"");
                at += 1 + len;
            }
            '%' => match percent_literal_len(&line[at..]) {
                Some(len) => {
                    out.push_str("\"\"");
                    at += len;
                }
                None => {
                    out.push('%');
                    at += 1;
                }
            },
            _ => {
                out.push(c);
                at += c.len_utf8();
            }
        }
    }
    out
}

/// The length of `%<type><open>…<close>` at the start of `text`, closing
/// delimiter included, or `None` when `text` opens no such literal or it
/// does not close on the line.
fn percent_literal_len(text: &str) -> Option<usize> {
    let mut chars = text.chars();
    chars.next();
    if !matches!(chars.next()?, 'q' | 'Q' | 'w' | 'W' | 'i' | 'I' | 'r') {
        return None;
    }
    let open = chars.next()?;
    let close = match open {
        '(' => ')',
        '[' => ']',
        '{' => '}',
        '<' => '>',
        c if c.is_ascii_punctuation() => c,
        _ => return None,
    };
    literal_body_len(&text[3..], open, close).map(|len| 3 + len)
}

/// The length of a literal body up to and including its `close`, read
/// with backslash escapes and, for a bracket pair, nesting. `None` when it
/// never closes.
fn literal_body_len(body: &str, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    let mut escaped = false;
    for (at, c) in body.char_indices() {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == close {
            if depth == 0 {
                return Some(at + c.len_utf8());
            }
            depth -= 1;
        } else if c == open {
            depth += 1;
        }
    }
    None
}

/// Every statement among the code lines `lines` that opens with the word
/// `depends_on`, joined across its continuation lines.
fn depends_on_statements(lines: &[String]) -> Vec<String> {
    let mut found = Vec::new();
    // The statement the previous line left open, whether it is a
    // `depends_on`, and how many brackets it has left open.
    let mut open: Option<(bool, String, i32)> = None;
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let segments: Vec<&str> = line.split(';').collect();
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
                || tail.ends_with([',', '(', '[', '{', '|', '.', '\\'])
                || tail.ends_with("&&");
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
        .trim_start()
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
    use super::{CustomBlockOsFacts, code_text, custom_block_os_facts};

    fn facts(other_os_block: bool, os_dependency: bool) -> CustomBlockOsFacts {
        CustomBlockOsFacts {
            other_os_block,
            os_dependency,
        }
    }

    /// The code of a line keeps everything outside its literals, writes
    /// each literal that closes on the line as `""`, and stops at a `#`
    /// outside them.
    #[test]
    fn code_text_blanks_literals_and_cuts_the_comment() {
        let cases = [
            (r#"url "x" # on_linux later"#, r#"url "" "#),
            ("caveats \"a # b\"", "caveats \"\""),
            ("x 'it''s'", "x \"\"\"\""),
            (r##"x "a\"#b" y"##, r#"x "" y"#),
            ("x %q(a(b)#c) y", "x \"\" y"),
            (
                "x %w[a b] %i{c} %r<d> %Q|e#| %W!f! %I/g/",
                "x \"\" \"\" \"\" \"\" \"\" \"\"",
            ),
            ("x %w[ unclosed", "x %w[ unclosed"),
            ("a % b # c", "a % b "),
            ("x \"open # never closed", "x "),
        ];
        for (line, want) in cases {
            assert_eq!(code_text(line), want, "{line}");
        }
    }

    /// What `custom_block_os_facts` reads from a `custom_block`, case by
    /// case. Both facts read the code of each line. A Linux side holds when
    /// its word appears there as a whole word. An OS dependency holds when
    /// a statement that opens with `depends_on`, read across its
    /// continuation lines, names an OS key or symbol. A wrong verdict
    /// renders a cask Homebrew refuses, so the over-matches below are the
    /// cases `depends_on_macos: true` exists for and the misses are the
    /// cases for `depends_on_macos: false`.
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
            // Over-matches: no Linux side in Ruby, so the rendered cask
            // loads on macOS and fails on Linux until `depends_on_macos:
            // true` is set.
            (
                "on_linux inside a %w[] list over several lines",
                "%w[\n on_linux\n]",
                facts(true, false),
            ),
            (
                "on_linux on a heredoc body line",
                "caveats <<~EOS\n  on_linux\nEOS\n",
                facts(true, false),
            ),
            (
                "a longer word holding on_linux or depends_on",
                "end_foo\non_linux_helper\nmy_on_system\ndepends_on_x :macos\n",
                facts(false, false),
            ),
            // A miss: a Linux side built at runtime, which `depends_on_macos:
            // false` exists for.
            (
                "an on_linux built at runtime",
                "send(:\"on_#{\"lin\"}ux\") do\n  url \"https://e.com/l.tar.gz\"\nend\n",
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
        for ending in [",", "(", "[", "{", "|", ".", "&&", "||", "\\"] {
            let block = format!("depends_on formula: \"x\" {ending}\n  macos: :ventura");
            assert_eq!(
                custom_block_os_facts(Some(&block)),
                facts(false, true),
                "{ending}"
            );
        }
    }
}
