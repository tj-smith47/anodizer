//! The literals a `custom_block` lexer reads: the frame each one opens and
//! the openers of a regex and a percent literal.

/// A literal, or the code of an interpolation inside one, that the lexer
/// is in.
#[derive(Debug)]
pub(super) enum Frame {
    /// A string, percent literal or regex, read up to `close`. A bracket
    /// pair nests, counted in `depth`.
    Literal {
        open: char,
        close: char,
        depth: usize,
        interpolates: bool,
        /// What the literal is and the line it opens on, for
        /// [`Unreadable`](super::Unreadable).
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

/// A literal delimited by `open` and `close`, opened on line `line`.
pub(super) fn literal(
    open: char,
    close: char,
    interpolates: bool,
    kind: &'static str,
    line: usize,
) -> Frame {
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
pub(super) fn regex(line: usize) -> Frame {
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
/// a modulo or a modulo assignment. A typed literal (`%q a `, `%q` at the
/// end of the line) takes a space or the end of the line anywhere it opens.
/// A literal delimited by the end of the line holds the next line, and a
/// list (`%w`, `%i`) delimited by whitespace never closes, since Ruby reads
/// that whitespace as a word separator.
pub(super) fn percent_literal(text: &[char], line: usize, bare: bool) -> Option<(usize, Frame)> {
    let (kind, at) = match text.get(1) {
        Some(c @ ('q' | 'Q' | 'w' | 'W' | 'i' | 'I' | 'r' | 's' | 'x')) => (Some(*c), 2),
        _ => (None, 1),
    };
    let list = matches!(kind, Some('w' | 'W' | 'i' | 'I'));
    let delimiter = text.get(at).copied();
    let (open, close) = match delimiter {
        None | Some(' ' | '\t') if list => ('\0', '\0'),
        None if kind.is_none() && !bare => return None,
        None => ('\n', '\n'),
        Some('(') => ('(', ')'),
        Some('[') => ('[', ']'),
        Some('{') => ('{', '}'),
        Some('<') => ('<', '>'),
        Some(' ' | '\t' | '=') if kind.is_none() && !bare => return None,
        Some(c) if matches!(c, ' ' | '\t') || c.is_ascii_punctuation() => (c, c),
        _ => return None,
    };
    let interpolates = !matches!(kind, Some('q' | 'w' | 'i' | 's'));
    let mut frame = literal(open, close, interpolates, "percent literal", line);
    if kind == Some('r')
        && let Frame::Literal { body, .. } = &mut frame
    {
        *body = Some(String::new());
    }
    Some((at + usize::from(delimiter.is_some()), frame))
}
