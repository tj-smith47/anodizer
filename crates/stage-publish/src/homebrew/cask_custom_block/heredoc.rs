//! The heredocs of a `custom_block`: their openers and the lines that close
//! them.

use super::is_word_char;

/// A heredoc whose body begins on the line after its opener.
pub(super) struct Heredoc {
    pub(super) tag: String,
    /// Opened by `<<~` or `<<-`: the closing tag may be indented.
    pub(super) indented: bool,
    /// Opened by `<<~`.
    pub(super) squiggly: bool,
    /// The line of the opener.
    pub(super) line: usize,
    /// It was opened on its line after an ambiguous opener.
    pub(super) guessed: bool,
    /// Its tag is not single-quoted, so a `\` at the end of a body line
    /// joins the next line to it and `#{…}` in a body line is code.
    pub(super) interpolates: bool,
    /// The body line before ended in an odd run of `\`, so the current line
    /// is joined to it and cannot close the heredoc.
    joined: bool,
    /// The body line before ended inside a `#{…}`, whose code reads on into
    /// the next line: the depth in the lexer's stack of the body's frame.
    pub(super) interpolating: Option<usize>,
}

impl Heredoc {
    /// Read body line `line`: true when it is the closing tag.
    pub(super) fn closes_on(&mut self, line: &str) -> bool {
        let joined = std::mem::take(&mut self.joined);
        let tag = if self.indented {
            line.trim_start()
        } else {
            line
        };
        if !joined && tag == self.tag {
            return true;
        }
        self.joined = self.interpolates && joins_next_line(line);
        false
    }
}

/// True when `line` ends in an odd run of `\`: the last one escapes the
/// newline, so the next line is joined to it.
pub(super) fn joins_next_line(line: &str) -> bool {
    (line.len() - line.trim_end_matches('\\').len()) % 2 == 1
}

/// The heredoc opened by the text after a `<<` on line `line`: the length
/// of that text and the heredoc, or `None` when the `<<` is a shift.
pub(super) fn heredoc_opener(text: &[char], line: usize) -> Option<(usize, Heredoc)> {
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
            interpolates: first != '\'',
            joined: false,
            interpolating: None,
        },
    ))
}
