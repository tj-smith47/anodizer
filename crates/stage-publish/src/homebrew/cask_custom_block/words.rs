//! The words and short tokens a `custom_block` lexer reads: what the code
//! before an operator makes of it, the words that take an argument, the
//! method names and symbols spelled as operators, the spelling of a number
//! and the `?x` character literal.

use super::is_word_char;
use super::names::Scopes;

/// Ruby keywords and the common methods after which a `<<`, `%`, `?` or `/`
/// opens a literal.
const OPERAND_WORDS: &[&str] = &[
    "abort",
    "and",
    "appcast",
    "begin",
    "break",
    "case",
    "colorpkg",
    "do",
    "else",
    "elsif",
    "ensure",
    "exec",
    "exit",
    "extend",
    "fail",
    "format",
    "if",
    "in",
    "include",
    "lambda",
    "loop",
    "next",
    "not",
    "odebug",
    "odie",
    "ohai",
    "onoe",
    "opoo",
    "or",
    "p",
    "print",
    "printf",
    "proc",
    "puts",
    "raise",
    "regex",
    "require",
    "rescue",
    "return",
    "sprintf",
    "strategy",
    "super",
    "system",
    "system_command",
    "then",
    "unless",
    "until",
    "warn",
    "when",
    "while",
    "yield",
];

/// Every method the cask DSL defines, as `Cask::DSL::DSL_METHODS` lists
/// them in Homebrew 7.0.8 (`Library/Homebrew/cask/dsl.rb`): the 42 names
/// written out in `DSL_METHODS`, the `dsl_key` of each of the 32
/// `ORDINARY_ARTIFACT_CLASSES`, the `dsl_key` and `uninstall_dsl_key` of
/// the 2 `ARTIFACT_BLOCK_CLASSES` (`preflight`, `uninstall_preflight`,
/// `postflight`, `uninstall_postflight`) and the `dsl_key` of the 4
/// `INSTALL_STEP_ARTIFACT_CLASSES`, 82 names in all. A stanza takes an
/// argument, so a literal opener after one opens a literal.
pub(super) const CASK_DSL_WORDS: &[&str] = &[
    "app",
    "app_image",
    "appdir",
    "arch",
    "artifact",
    "artifacts",
    "audio_unit_plugin",
    "auto_updates",
    "autobump?",
    "bash_completion",
    "binary",
    "caveats",
    "colorpicker",
    "command_wrapper",
    "conflicts_with",
    "container",
    "depends_on",
    "depends_on_set_in_block?",
    "deprecate!",
    "deprecate_args",
    "deprecated?",
    "deprecation_date",
    "deprecation_reason",
    "deprecation_replacement_cask",
    "deprecation_replacement_formula",
    "desc",
    "dictionary",
    "disable!",
    "disable_args",
    "disable_date",
    "disable_reason",
    "disable_replacement_cask",
    "disable_replacement_formula",
    "disabled?",
    "fish_completion",
    "font",
    "generate_completions_from_executable",
    "generated_script",
    "homepage",
    "homepage_browsed",
    "input_method",
    "installer",
    "internet_plugin",
    "keyboard_layout",
    "language",
    "livecheck",
    "livecheck_defined?",
    "manpage",
    "mdimporter",
    "name",
    "no_autobump!",
    "no_autobump_message",
    "on_os_blocks_exist?",
    "on_system_block_min_os",
    "on_system_blocks_exist?",
    "os",
    "pkg",
    "postflight",
    "postflight_steps",
    "preflight",
    "preflight_steps",
    "prefpane",
    "pwsh_completion",
    "qlplugin",
    "rename",
    "screen_saver",
    "service",
    "sha256",
    "stage_only",
    "staged_path",
    "suite",
    "uninstall",
    "uninstall_postflight",
    "uninstall_postflight_steps",
    "uninstall_preflight",
    "uninstall_preflight_steps",
    "url",
    "version",
    "vst3_plugin",
    "vst_plugin",
    "zap",
    "zsh_completion",
];

/// Keywords after which a `{` opens a hash, and a `%`, `/` or `<<`
/// written straight against them opens a literal (`if%w[a]`). `class`,
/// `for` and `module` are left out: no literal follows them in valid Ruby.
pub(super) const HASH_AFTER: &[&str] = &[
    "and", "begin", "break", "case", "do", "else", "elsif", "ensure", "if", "in", "next", "not",
    "or", "rescue", "return", "then", "unless", "until", "when", "while", "yield",
];

/// Whether the keyword `word` leaves Ruby where an operand must stand, so
/// a `%` after it opens a literal whatever delimits it (`if% a `, `do% a
/// `). `not` and `yield` read like a method call.
pub(super) fn leaves_operand(word: &str) -> bool {
    HASH_AFTER.contains(&word) && !matches!(word, "not" | "yield")
}

/// Whether a `<<`, `%`, `?` or `/` after `word` opens a literal.
pub(super) fn takes_operand(word: &str) -> bool {
    OPERAND_WORDS.contains(&word) || CASK_DSL_WORDS.contains(&word)
}

/// The method names spelled as operators, longest first so a name is read
/// whole (`<=>` before `<=` before `<`).
const OPERATOR_METHODS: &[&str] = &[
    "[]=", "<=>", "===", "[]", "**", "==", "=~", "!=", "!~", "<<", ">>", "<=", ">=", "+@", "-@",
    "!@", "~@", "+", "-", "*", "/", "%", "<", ">", "!", "~", "&", "|", "^", "`",
];

/// The length of the operator method name at the start of `text`, which
/// follows a `.`, `&.` or `::` (`x.%`, `a&.!~`), or `None` when it starts
/// with none.
pub(super) fn operator_method_len(text: &[char]) -> Option<usize> {
    OPERATOR_METHODS
        .iter()
        .find(|name| text.len() >= name.len() && name.chars().zip(text).all(|(a, b)| a == *b))
        .map(|name| name.len())
}

/// The length of the symbol name at the start of `text`, which follows a
/// `:`, when the name is spelled as an operator (`:+`, `:<=>`, `:[]=`) or
/// ends in `=`, `?` or `!` (`:a=`, `:a?`), or `None` for any other name.
pub(super) fn symbol_len(text: &[char]) -> Option<usize> {
    if let Some(len) = operator_method_len(text) {
        return Some(len);
    }
    if !text
        .first()
        .is_some_and(|&c| is_word_char(c) && !c.is_ascii_digit())
    {
        return None;
    }
    let name = text.iter().take_while(|&&c| is_word_char(c)).count();
    let next = text.get(name + 1).copied();
    match text.get(name) {
        // `:a==b` compares and `:a=>1` is a pair, as `:a!=b` compares.
        Some('?' | '!') if next != Some('=') => Some(name + 1),
        Some('=') if !matches!(next, Some('=' | '~' | '>')) => Some(name + 1),
        _ => None,
    }
}

/// Whether `c` extends the number `word` (`0x1f`, `1_000`, `1e5`, `2r`):
/// a keyword written against a number starts a word of its own (`2if`).
pub(super) fn number_continues(word: &str, c: char, next: Option<char>) -> bool {
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
/// `None` when the `?` is not one (`a ? b : c`, `?ab`). `operand` is
/// whether the `?` stands where an operand must, and not after a word that
/// may be a local.
pub(super) fn char_literal_len(text: &[char], operand: bool) -> Option<usize> {
    let c = *text.get(1).filter(|c| !c.is_whitespace())?;
    let len = if c == '\\' { 3 } else { 2 };
    // A letter, digit or `_` followed straight by a word character is no
    // literal (`?ab`). Where an operand must stand any other character
    // opens one (`?"if`); after a word that may be a local a quote before
    // a word is read as a ternary before a string (`x ?"a"`), while `?#if`
    // is still a literal before a modifier.
    let blocked = if operand {
        c.is_ascii_alphanumeric() || c == '_'
    } else {
        !matches!(c, '#' | '\\')
    };
    if blocked && text.get(len).is_some_and(|&next| is_word_char(next)) {
        return None;
    }
    Some(len.min(text.len()))
}

/// What the code before a `<<`, `%`, `?` or `/` makes of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Position {
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
/// a name ending in `?` or `!` followed by a space, an operator, `(`, `,`
/// or the start of a statement leaves an operand position, and any other
/// word followed by a space a guess.
pub(super) fn position(
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
            // No local or parameter is spelled with a `?` or `!`, so the
            // word is a method call and a space after it leaves its
            // argument (`foo? <<E`).
            if spaced && word.ends_with(['?', '!']) {
                return Position::Operand;
            }
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
