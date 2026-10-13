//! The names a `custom_block` binds as local variables and parameters, and
//! the scopes they are visible in.

use std::collections::HashMap;

use super::{is_local_name, is_word_char};

/// What a level of nesting does to the names bound inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LevelKind {
    /// A `do … end` or `{ … }` block: it sees the names around it, and the
    /// names bound inside it end with it.
    Block,
    /// A `def`, `class` or `module` body: it sees none of the names around
    /// it.
    Gate,
    /// An `if`, `unless`, `while`, `until`, `case`, `begin` or `for` body:
    /// the names bound inside it belong to the scope around it.
    Plain,
}

/// One level of nesting that an `end` or a `}` closes.
pub(super) struct Level {
    /// The names bound at this level, when it is a scope.
    names: Vec<String>,
    /// The innermost scope (this level or one around it) that binds names.
    scope: usize,
    /// The innermost `def`, `class` or `module` level, or the top.
    gate: usize,
}

/// The nesting of a block and the local variables and parameters visible
/// at each point of it.
pub(super) struct Scopes {
    pub(super) levels: Vec<Level>,
    /// Per name, the levels binding it, innermost last, each with whether
    /// the binding is a parameter.
    bound: HashMap<String, Vec<(usize, bool)>>,
}

impl Default for Scopes {
    fn default() -> Self {
        Scopes {
            levels: vec![Level {
                names: Vec::new(),
                scope: 0,
                gate: 0,
            }],
            bound: HashMap::new(),
        }
    }
}

impl Scopes {
    pub(super) fn top(&self) -> &Level {
        &self.levels[self.levels.len() - 1]
    }

    pub(super) fn push(&mut self, kind: LevelKind) {
        let at = self.levels.len();
        let (scope, gate) = (self.top().scope, self.top().gate);
        self.levels.push(Level {
            names: Vec::new(),
            scope: if kind == LevelKind::Plain { scope } else { at },
            gate: if kind == LevelKind::Gate { at } else { gate },
        });
    }

    /// Close the innermost level and drop the names bound at it. An `end`
    /// with nothing open is passed over.
    pub(super) fn pop(&mut self) {
        if self.levels.len() == 1 {
            return;
        }
        let Some(level) = self.levels.pop() else {
            return;
        };
        for name in level.names {
            if let Some(levels) = self.bound.get_mut(&name) {
                levels.pop();
                if levels.is_empty() {
                    self.bound.remove(&name);
                }
            }
        }
    }

    /// Bind `name` in the innermost scope, as a parameter when `param`. A
    /// local assignment to a parameter makes it a local.
    pub(super) fn bind(&mut self, name: &str, param: bool) {
        let at = self.top().scope;
        let levels = self.bound.entry(name.to_string()).or_default();
        match levels.last_mut() {
            Some((level, was_param)) if *level == at => *was_param &= param,
            _ => {
                levels.push((at, param));
                self.levels[at].names.push(name.to_string());
            }
        }
    }

    /// Whether `name` is visible here: `Some(true)` for a parameter,
    /// `Some(false)` for a local, `None` when it is neither.
    pub(super) fn lookup(&self, name: &str) -> Option<bool> {
        let &(level, param) = self.bound.get(name)?.last()?;
        (level >= self.top().gate).then_some(param)
    }
}

/// The names `code`, the statement before an `=`, assigns: `a` from `a`
/// or `a ||` (and the other operator assignments), `a`, `b` and `c` from
/// `a, *b, c`, `(a, b)` and `a, (b, c)`, and `a` from `a, =`. Empty when
/// `code` is anything else.
pub(super) fn assigned_names(code: &str) -> Vec<String> {
    let target = code
        .trim()
        .trim_end_matches(['|', '&', '+', '-', '*', '/', '%', '<', '>', '^'])
        .trim();
    let mut names = Vec::new();
    if target_names(target, &mut names, 0) {
        names
    } else {
        Vec::new()
    }
}

/// The local an operator assignment (`b ||= 1`, `b <<= 1`) assigns, when
/// `code` is the code before its `=`: the name written before the
/// operator. A name after `.`, `&.` or `::`, or with a sigil, is a method
/// or another kind of variable and binds no local.
pub(super) fn operator_target(code: &str) -> Option<&str> {
    let before = code
        .trim_end_matches(['|', '&', '+', '-', '*', '/', '%', '<', '>', '^'])
        .trim_end();
    let start = before
        .char_indices()
        .rev()
        .find(|&(_, c)| !is_word_char(c))
        .map_or(0, |(at, c)| at + c.len_utf8());
    let name = &before[start..];
    let lead = before[..start].trim_end().chars().next_back();
    (is_local_name(name) && !matches!(lead, Some('.' | ':' | '@' | '$'))).then_some(name)
}

/// Add the names of the assignment target `target` to `names`, reading a
/// parenthesized group as a nested target. False when `target` is not one,
/// or nests deeper than [`MAX_TARGET_DEPTH`] groups.
fn target_names(target: &str, names: &mut Vec<String>, depth: usize) -> bool {
    let target = target.trim();
    let target = wrapped(target).unwrap_or(target);
    if depth > MAX_TARGET_DEPTH {
        return false;
    }
    let parts = split_top_level(target);
    let count = parts.len();
    for (n, part) in parts.into_iter().enumerate() {
        let part = part.trim();
        let name = part.trim_start_matches('*');
        // `a, = list` and a bare `*` bind nothing in their place.
        if name.is_empty() && (part.starts_with('*') || (n + 1 == count && count > 1)) {
            continue;
        }
        if name.starts_with('(') {
            // Only a group that closes is a nested target; `(a` is not one.
            match wrapped(name) {
                Some(inner) if target_names(inner, names, depth + 1) => {}
                _ => return false,
            }
        } else if is_local_name(name) && name.chars().all(is_word_char) {
            names.push(name.to_string());
        } else {
            return false;
        }
    }
    true
}

/// How many parenthesized groups an assignment target may nest, which keeps
/// the recursion bounded on a block of thousands of `(`.
const MAX_TARGET_DEPTH: usize = 32;

/// The text inside `text` when one pair of parentheses encloses all of it.
fn wrapped(text: &str) -> Option<&str> {
    let inner = text.strip_prefix('(')?.strip_suffix(')')?;
    let mut depth = 0i32;
    for c in inner.chars() {
        depth += match c {
            '(' | '[' | '{' => 1,
            ')' | ']' | '}' => -1,
            _ => 0,
        };
        if depth < 0 {
            return None;
        }
    }
    (depth == 0).then_some(inner)
}

/// `text` split at each `,` outside brackets.
fn split_top_level(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (at, c) in text.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&text[start..at]);
                start = at + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}

/// The names a `for … in` statement `code`, read up to `in`, binds.
pub(super) fn for_names(code: &str) -> Vec<String> {
    code.trim_start()
        .strip_prefix("for ")
        .and_then(|rest| rest.trim_end().strip_suffix(" in"))
        .map(|names| assigned_names(names.trim()))
        .unwrap_or_default()
}

/// Bind the names a whole statement `code` declares: the variable of a
/// `rescue … => e`, the parameters of a `def name a, b` written without
/// parentheses, and the names a pattern binds in a `case`'s `in` branch or
/// a rightward assignment (`value => name`).
pub(super) fn bind_statement(code: &str, scopes: &mut Scopes) {
    let code = code.trim();
    if code.starts_with("rescue") {
        if let Some((_, name)) = code.rsplit_once("=>") {
            for name in assigned_names(name) {
                scopes.bind(&name, false);
            }
        }
    } else if let Some(rest) = code.strip_prefix("def ") {
        if let Some((name, list)) = rest.trim_start().split_once(char::is_whitespace)
            && !name.contains('(')
        {
            for name in param_names(list) {
                scopes.bind(&name, true);
            }
        }
    } else if let Some(pattern) = in_pattern(code).or_else(|| rightward_pattern(code)) {
        for name in pattern_names(pattern) {
            scopes.bind(&name, false);
        }
    }
}

/// The pattern after the first `in` outside brackets in `code`: a `case`
/// branch (`in [a, b]`) or a one-line match (`value in {name:}`). A `for`
/// loop's `in` is no pattern, and neither is a method (`.in`), a symbol
/// (`:in`) or a key (`in:`).
fn in_pattern(code: &str) -> Option<&str> {
    if code.starts_with("for ") {
        return None;
    }
    let bytes = code.as_bytes();
    let is_word = |at: usize| {
        bytes
            .get(at)
            .is_some_and(|&b| b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80)
    };
    let mut depth = 0i32;
    for (at, &b) in bytes.iter().enumerate() {
        match b {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'i' if depth == 0
                && code[at..].starts_with("in")
                && !is_word(at + 2)
                && (at == 0 || !(is_word(at - 1) || matches!(bytes[at - 1], b'.' | b':')))
                && bytes.get(at + 2) != Some(&b':') =>
            {
                return Some(&code[at + 2..]);
            }
            _ => {}
        }
    }
    None
}

/// The pattern of a rightward assignment `value => pattern`: the text after
/// the last `=>` outside brackets, when the value before it is one
/// expression with no space outside brackets (a call such as
/// `url "x", :using => :post` passes a hash).
fn rightward_pattern(code: &str) -> Option<&str> {
    let mut depth = 0i32;
    let mut arrow = None;
    let bytes = code.as_bytes();
    for (at, &b) in bytes.iter().enumerate() {
        match b {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'=' if depth == 0 && bytes.get(at + 1) == Some(&b'>') => arrow = Some(at),
            _ => {}
        }
    }
    let at = arrow?;
    let value = code[..at].trim();
    let mut depth = 0i32;
    let one_expression = !value.is_empty()
        && value.chars().all(|c| {
            depth += match c {
                '(' | '[' | '{' => 1,
                ')' | ']' | '}' => -1,
                _ => 0,
            };
            depth > 0 || !c.is_whitespace()
        });
    one_expression.then(|| &code[at + 2..])
}

/// The names a pattern binds: each bare lowercase name that is no method
/// call, no pinned `^name` and no key with a value pattern after it, up to
/// a `then`, `if` or `unless`.
fn pattern_names(pattern: &str) -> Vec<String> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut names = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let starts = is_local_name(&chars[i].to_string())
            && (i == 0
                || !(is_word_char(chars[i - 1])
                    || matches!(chars[i - 1], '.' | ':' | '^' | '@' | '$')));
        if !starts {
            i += 1;
            continue;
        }
        let from = i;
        while i < chars.len() && is_word_char(chars[i]) {
            i += 1;
        }
        let name: String = chars[from..i].iter().collect();
        if matches!(name.as_str(), "then" | "if" | "unless") {
            break;
        }
        let bound = match chars.get(i) {
            Some(':') if chars.get(i + 1) != Some(&':') => matches!(
                chars[i + 1..].iter().find(|c| !c.is_whitespace()),
                None | Some(',' | '}' | ')')
            ),
            Some('(' | '.' | '?' | '!' | ':') => false,
            _ => !matches!(
                name.as_str(),
                "nil" | "true" | "false" | "self" | "in" | "and" | "or" | "not"
            ),
        };
        if bound {
            names.push(name);
        }
    }
    names
}

/// The names of the named groups `(?<name>…)` in the regex text `body`
/// that can be locals.
pub(super) fn capture_names(body: &str) -> Vec<String> {
    body.match_indices("(?<")
        .filter_map(|(at, _)| {
            let rest = &body[at + 3..];
            let name: String = rest.chars().take_while(|&c| is_word_char(c)).collect();
            (is_local_name(&name) && rest[name.len()..].starts_with('>')).then_some(name)
        })
        .collect()
}

/// The names in a parameter list: each entry's leading name after any
/// `*`, `**`, `&` or `(`, so a default value or a keyword's `:` is passed
/// over. A block's own locals after `;` (`|x; w|`) count too.
pub(super) fn param_names(list: &str) -> Vec<String> {
    list.split([',', ';'])
        .filter_map(|entry| {
            let entry = entry.trim_start_matches(|c: char| c.is_whitespace() || "*&(".contains(c));
            let name: String = entry.chars().take_while(|&c| is_word_char(c)).collect();
            is_local_name(&name).then_some(name)
        })
        .collect()
}
