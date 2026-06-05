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

/// Locate the `"<key>"` key and return the byte index of the `[` that opens the
/// array immediately following it. The search for `[` runs through the
/// `JsonScan` state machine, so a `[` inside an intervening string or `//`
/// comment is skipped.
pub(crate) fn find_array_open_after(text: &str, key: &str) -> anyhow::Result<usize> {
    let needle = format!("\"{key}\"");
    let key_at = text
        .find(&needle)
        .ok_or_else(|| anyhow::anyhow!("no `{key}` key found"))?;
    let bytes = text.as_bytes();
    let mut scan = JsonScan::new();
    // Resume scanning just past the key token's closing quote so the key's own
    // quotes do not desync the string-state tracker.
    let resume = key_at + needle.len();
    for (i, &b) in bytes.iter().enumerate().skip(resume) {
        if let Some(b'[') = scan.step(b) {
            return Ok(i);
        }
    }
    anyhow::bail!("`{key}` key is not followed by an array")
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
    let bytes = text.as_bytes();
    let mut scan = JsonScan::new();
    let mut depth = 0i32;
    for (i, &b) in bytes.iter().enumerate().skip(open_bracket_idx) {
        if let Some(s) = scan.step(b) {
            match s {
                b'[' => depth += 1,
                b']' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(i);
                    }
                }
                _ => {}
            }
        }
    }
    anyhow::bail!("array opened at byte {open_bracket_idx} is not closed")
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
