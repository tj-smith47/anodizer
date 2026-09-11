//! String- and comment-aware byte scanner for SchemaStore's JSON/JSONC files.
//!
//! SchemaStore's `catalog.json` (plain JSON) and `schema-validation.jsonc`
//! (JSONC with `//` line comments) are both edited *textually* by the publisher
//! so prettier-managed formatting and comments survive review. Locating an
//! array's `[`/`]` span or an element therefore cannot rely on `serde_json`
//! (which would reorder/strip); it requires a scanner that counts structural
//! `{}[]` only when they lie outside any string literal or comment. This module
//! is that scanner stack — pure byte arithmetic, no catalog domain types.

/// Cursor that walks JSON/JSONC bytes while tracking whether the current
/// position is inside a string literal or a `//` line comment, so that
/// structural `{}[]"` only count when they are *outside* both.
///
/// JSON structural characters are all ASCII, so byte iteration is safe and the
/// recorded indices always land on UTF-8 char boundaries. `catalog.json` has
/// no comments (so comment-skipping is inert there); `schema-validation.jsonc`
/// does, and a `]` inside a `//` comment must not be mistaken for structural.
pub(crate) struct JsonScan {
    pub(crate) in_string: bool,
    escaped: bool,
    in_comment: bool,
    /// The previous byte was a `/` outside a string/comment — a second `/`
    /// now would open a `//` line comment.
    prev_slash: bool,
}

impl JsonScan {
    pub(crate) fn new() -> Self {
        Self {
            in_string: false,
            escaped: false,
            in_comment: false,
            prev_slash: false,
        }
    }

    /// Advance over one byte and report whether it is a *structural* character
    /// (i.e. outside any string literal or `//` comment). Returns `Some(b)` for
    /// a structural `{}[]` and `None` otherwise.
    pub(crate) fn step(&mut self, b: u8) -> Option<u8> {
        if self.in_comment {
            // A `//` line comment runs to the next newline; nothing inside it
            // is structural and `"` does not open a string.
            if b == b'\n' {
                self.in_comment = false;
            }
            return None;
        }
        if self.in_string {
            if self.escaped {
                // The previous byte was a backslash; this byte is consumed as
                // the escape payload and cannot end the string.
                self.escaped = false;
            } else if b == b'\\' {
                self.escaped = true;
            } else if b == b'"' {
                self.in_string = false;
            }
            return None;
        }
        if self.prev_slash {
            self.prev_slash = false;
            if b == b'/' {
                self.in_comment = true;
                return None;
            }
        }
        match b {
            b'"' => {
                self.in_string = true;
                None
            }
            b'/' => {
                self.prev_slash = true;
                None
            }
            b'{' | b'}' | b'[' | b']' => Some(b),
            _ => None,
        }
    }
}

/// The same bytes with every `//` line comment removed, so a JSONC fragment can
/// be handed to a plain JSON parser.
///
/// String literals are left untouched — a `//` inside one is data, not a
/// comment — and the newline that ends a comment is kept, so byte offsets of
/// later lines shift only by the comment text itself.
pub(crate) fn strip_line_comments(text: &str) -> String {
    let mut scan = JsonScan::new();
    let mut out: Vec<u8> = Vec::with_capacity(text.len());
    // A `/` is emitted only once the NEXT byte proves it is not opening a
    // comment, so the scanner's own one-byte lookahead is mirrored here.
    let mut pending_slash = false;
    for &b in text.as_bytes() {
        scan.step(b);
        if scan.in_comment {
            pending_slash = false;
            continue;
        }
        if pending_slash {
            out.push(b'/');
            pending_slash = false;
        }
        if scan.prev_slash {
            pending_slash = true;
            continue;
        }
        out.push(b);
    }
    if pending_slash {
        out.push(b'/');
    }
    // Only ASCII comment bytes are dropped, so what remains is still UTF-8.
    String::from_utf8(out).unwrap_or_else(|_| text.to_string())
}

/// Locate the `"<key>"` key and return the byte index of the `[` that opens the
/// array immediately following it. Both the key hunt and the `[` hunt run
/// through the `JsonScan` state machine, and the match must sit in KEY position
/// (followed by `:`), so a `"<key>"` mentioned inside a `//` comment, or spelt
/// by a string VALUE, is skipped — as is a `[` inside either.
pub(crate) fn find_array_open_after(text: &str, key: &str) -> anyhow::Result<usize> {
    find_open_after(text, key, b'[', "an array")
}

/// Locate the `"<key>"` key and return the byte index of the `{` that opens the
/// object immediately following it. Comment- and string-aware, like
/// [`find_array_open_after`].
pub(crate) fn find_object_open_after(text: &str, key: &str) -> anyhow::Result<usize> {
    find_open_after(text, key, b'{', "an object")
}

fn find_open_after(text: &str, key: &str, open_b: u8, what: &str) -> anyhow::Result<usize> {
    let bytes = text.as_bytes();
    let mut scan = JsonScan::new();
    let mut str_start: Option<usize> = None;
    let mut was_in_string = false;
    let mut found_key = false;
    for (i, &b) in bytes.iter().enumerate() {
        let structural = scan.step(b);
        if found_key {
            // Past the key: the first structural open of the wanted kind is the
            // container it introduces.
            if structural == Some(open_b) {
                return Ok(i);
            }
        } else if scan.in_string && !was_in_string {
            str_start = Some(i + 1);
        } else if !scan.in_string
            && was_in_string
            && let Some(start) = str_start.take()
            && decode_json_string(&text[start..i]).as_deref() == Some(key)
            && is_object_key(bytes, i)
        {
            found_key = true;
        }
        was_in_string = scan.in_string;
    }
    if found_key {
        anyhow::bail!("`{key}` key is not followed by {what}")
    }
    anyhow::bail!("no `{key}` key found")
}

/// Whether the string literal whose closing quote sits at `quote` is an object
/// KEY, i.e. the next non-whitespace byte after it is the `:` that separates a
/// key from its value. A string VALUE that happens to spell the key name is
/// therefore not mistaken for the key itself.
fn is_object_key(bytes: &[u8], quote: usize) -> bool {
    bytes[quote + 1..].iter().find(|b| !b.is_ascii_whitespace()) == Some(&b':')
}

/// Locate the `"schemas"` key and return the byte index of its opening `[`.
pub(crate) fn find_schemas_array_open(catalog: &str) -> anyhow::Result<usize> {
    find_array_open_after(catalog, "schemas")
}

/// Starting at the `[` at `open_bracket_idx`, return the byte index of the `]`
/// that closes it at depth 0.
///
/// Tracks `[`/`]` nesting depth via `JsonScan`, counting only brackets outside
/// any string literal or `//` comment (a `]` in a `description` value or a
/// JSONC comment must not affect depth). The closing index is the `]` that
/// brings the depth back to zero.
pub(crate) fn find_bracket_close(text: &str, open_bracket_idx: usize) -> anyhow::Result<usize> {
    find_close(text, open_bracket_idx, b'[', b']')
}

/// Starting at the `{` at `open_brace_idx`, return the byte index of the `}`
/// that closes it at depth 0. Comment- and string-aware, like
/// [`find_bracket_close`].
pub(crate) fn find_brace_close(text: &str, open_brace_idx: usize) -> anyhow::Result<usize> {
    find_close(text, open_brace_idx, b'{', b'}')
}

fn find_close(text: &str, open_idx: usize, open_b: u8, close_b: u8) -> anyhow::Result<usize> {
    let bytes = text.as_bytes();
    let mut scan = JsonScan::new();
    let mut depth = 0i32;
    for (i, &b) in bytes.iter().enumerate().skip(open_idx) {
        if let Some(s) = scan.step(b) {
            if s == open_b {
                depth += 1;
            } else if s == close_b {
                depth -= 1;
                if depth == 0 {
                    return Ok(i);
                }
            }
        }
    }
    anyhow::bail!("bracket opened at byte {open_idx} is not closed")
}

/// Byte span of the `"<key>": <value>` member inside the object whose braces
/// sit at `open`/`close`, or `None` when the object holds no such member.
/// `start` is the index of the key's opening quote; `end` is one past the last
/// byte of its value. The value must be an object or an array — the only
/// member shapes the publisher rewrites.
///
/// Members are enumerated by brace/bracket-balanced scanning of the object's
/// interior (string- and comment-aware), so a `"key":` appearing inside a
/// nested object, a string value, or a `//` comment is never mistaken for a
/// top-level member of this object.
pub(crate) fn find_object_member_span(
    text: &str,
    open: usize,
    close: usize,
    key: &str,
) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut scan = JsonScan::new();
    let mut depth = 0i32;
    // The most recent string literal that closed at depth 0 — the candidate
    // member key, held until its `:` value is seen.
    let mut pending: Option<(usize, String)> = None;
    let mut str_start: Option<usize> = None;
    let mut was_in_string = false;
    let mut member_start: Option<usize> = None;
    for i in open + 1..close {
        let b = bytes[i];
        let structural = scan.step(b);
        if scan.in_string && !was_in_string {
            str_start = Some(i + 1);
        } else if !scan.in_string
            && was_in_string
            && let Some(s) = str_start.take()
            && depth == 0
            && member_start.is_none()
        {
            pending = decode_json_string(&text[s..i]).map(|d| (s - 1, d));
        }
        was_in_string = scan.in_string;

        match structural {
            Some(b'{') | Some(b'[') => {
                if depth == 0
                    && member_start.is_none()
                    && pending.as_ref().is_some_and(|(_, k)| k == key)
                {
                    member_start = pending.as_ref().map(|(s, _)| *s);
                }
                depth += 1;
            }
            Some(b'}') | Some(b']') => {
                depth -= 1;
                if depth == 0
                    && let Some(s) = member_start
                {
                    return Some((s, i + 1));
                }
            }
            _ => {}
        }
    }
    None
}

/// Return the byte index of the `]` that closes the `schemas` array.
pub(crate) fn find_array_close(catalog: &str) -> anyhow::Result<usize> {
    let open = find_schemas_array_open(catalog)?;
    find_bracket_close(catalog, open)
}

/// True when the JSON/JSONC array named `key` contains the exact string element
/// `value`. Comment- and string-aware (reuses the [`JsonScan`] stack), so a
/// `value` appearing inside a `//` comment or another string never counts.
///
/// Returns `false` — never an error — when the `key` array is absent or
/// malformed: the schemastore change-decision treats "couldn't confirm
/// membership" as "not allowlisted ⇒ change needed", which is the conservative
/// direction (it never yields a false no-op).
pub(crate) fn jsonc_array_contains(jsonc: &str, key: &str, value: &str) -> bool {
    let Ok(open) = find_array_open_after(jsonc, key) else {
        return false;
    };
    let Ok(close) = find_bracket_close(jsonc, open) else {
        return false;
    };
    array_contains_element(&jsonc[open + 1..close], value)
}

/// True if the array interior contains a string element whose decoded value
/// exactly equals `name`. Element-exact (not substring): scans for `"..."`
/// tokens via `JsonScan` and compares each decoded literal, so `"foo-extra"`
/// does not match `foo`.
pub(crate) fn array_contains_element(interior: &str, name: &str) -> bool {
    let bytes = interior.as_bytes();
    let mut scan = JsonScan::new();
    let mut start: Option<usize> = None;
    let mut was_in_string = false;
    for (i, &b) in bytes.iter().enumerate() {
        scan.step(b);
        if scan.in_string && !was_in_string {
            // String just opened on this `"`; the value begins at i+1.
            start = Some(i + 1);
        } else if !scan.in_string && was_in_string {
            // String just closed on this `"`; the value spans [start, i).
            if let Some(s) = start.take()
                && let Some(decoded) = decode_json_string(&interior[s..i])
                && decoded == name
            {
                return true;
            }
        }
        was_in_string = scan.in_string;
    }
    false
}

/// Decode a JSON string literal body (the bytes *between* the quotes), handling
/// the standard escapes. Returns `None` on a malformed escape — the caller then
/// treats the element as non-matching, which is safe (it triggers an insert).
pub(crate) fn decode_json_string(body: &str) -> Option<String> {
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            '/' => out.push('/'),
            'b' => out.push('\u{0008}'),
            'f' => out.push('\u{000C}'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'u' => {
                let hex: String = (&mut chars).take(4).collect();
                let cp = u32::from_str_radix(&hex, 16).ok()?;
                out.push(char::from_u32(cp)?);
            }
            _ => return None,
        }
    }
    Some(out)
}

/// The leading whitespace of the line containing byte index `idx`. Anchoring an
/// insert's indentation off the key line (rather than the `[` column) keeps the
/// edit prettier-correct whether the array is multi-line, empty, or written on
/// a single line.
pub(crate) fn line_indent(text: &str, idx: usize) -> String {
    let line_start = text[..idx].rfind('\n').map(|n| n + 1).unwrap_or(0);
    let rest = &text[line_start..];
    let ws_len = rest
        .find(|c: char| c != ' ' && c != '\t')
        .unwrap_or(rest.len());
    rest[..ws_len].to_string()
}

/// Detect the leading-whitespace indent of the first existing array element by
/// finding the first structural `"` in the interior and measuring the
/// whitespace before it on its line. Returns `None` for an empty array.
pub(crate) fn first_element_indent(jsonc: &str, open: usize, interior: &str) -> Option<String> {
    let bytes = interior.as_bytes();
    let mut scan = JsonScan::new();
    for (i, &b) in bytes.iter().enumerate() {
        // `step` returns None for `"`, but flips `in_string` true on a string
        // open; detect that transition to find the first element.
        let before = scan.in_string;
        scan.step(b);
        if scan.in_string && !before {
            // Absolute byte index of this opening quote in `jsonc`.
            let abs = open + 1 + i;
            return Some(line_indent(jsonc, abs));
        }
    }
    None
}

/// True if the array interior holds at least one real element (a string
/// literal), as opposed to being empty or whitespace/comments only.
pub(crate) fn interior_has_element(interior: &str) -> bool {
    let mut scan = JsonScan::new();
    let mut was = false;
    for &b in interior.as_bytes() {
        scan.step(b);
        if scan.in_string && !was {
            return true;
        }
        was = scan.in_string;
    }
    false
}
