//! Rustdoc intra-doc links unwrapped to inline code for the generated
//! configuration reference.

/// Unwrap rustdoc intra-doc links — `` [`Path::to::item`] `` and
/// `` [`item`](rust::path) `` — to plain inline code.
///
/// schemars copies rustdoc verbatim into schema descriptions, but this
/// markdown pipeline has no rustdoc link resolver, so the bracket syntax
/// would render literally (e.g. ``[`Self::upload_concurrency`]``) or, for
/// the explicit-target form, as a link whose href is a rust path — broken
/// either way. A leading `Self::` is dropped — the reference tables already
/// scope fields to their section. Markdown links with a real URL / relative
/// / anchor target (``[`x`](https://…)``) and reference-style links
/// (``[`x`][ref]``) are left untouched: those render as real links.
pub(super) fn unwrap_rustdoc_links(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            // A code span is passed over whole, so a `[` or `]` inside one
            // (`` `[` ``) is never read as a link delimiter.
            let run = backtick_run(bytes, i);
            i = closing_backtick_run(bytes, i + run, run).map_or(i + run, |close| close + run);
            continue;
        }
        let link = (bytes[i] == b'[' && bytes.get(i + 1) == Some(&b'`'))
            .then(|| {
                let run = backtick_run(bytes, i + 1);
                let close = closing_backtick_run(bytes, i + 1 + run, run)?;
                (bytes.get(close + run) == Some(&b']')).then_some((run, close))
            })
            .flatten();
        let Some((run, close)) = link else {
            i += 1;
            continue;
        };
        let code = &s[i + 1 + run..close];
        let mut after = close + run + 1;
        let mut unwrap_to_code = true;
        match bytes.get(after) {
            // Explicit target: unwrap only when the target is a rust path,
            // and consume it; a real URL/relative/anchor target stays a link.
            Some(b'(') => match s[after..].find(')') {
                Some(close_rel) if is_rust_path_target(&s[after + 1..after + close_rel]) => {
                    after += close_rel + 1;
                }
                _ => unwrap_to_code = false,
            },
            // Reference-style link — leave for the renderer.
            Some(b'[') => unwrap_to_code = false,
            _ => {}
        }
        if unwrap_to_code {
            out.push_str(&s[copied..i]);
            out.push('`');
            out.push_str(code.strip_prefix("Self::").unwrap_or(code));
            out.push('`');
            copied = after;
        }
        i = after;
    }
    out.push_str(&s[copied..]);
    out
}

/// The length of the run of backticks starting at `at`.
fn backtick_run(bytes: &[u8], at: usize) -> usize {
    bytes[at..].iter().take_while(|&&b| b == b'`').count()
}

/// Where the next run of exactly `run` backticks at or after `from` starts,
/// the end of a code span opened by a run of that length.
fn closing_backtick_run(bytes: &[u8], from: usize, run: usize) -> Option<usize> {
    let mut at = from;
    while at < bytes.len() {
        if bytes[at] == b'`' {
            let len = backtick_run(bytes, at);
            if len == run {
                return Some(at);
            }
            at += len;
        } else {
            at += 1;
        }
    }
    None
}

/// True when a markdown link target is a rustdoc item path (`crate::x`,
/// `super::super::y`, `Self::z`, or a bare identifier) rather than a real
/// URL, relative file path, or `#anchor` — i.e. when the "link" only means
/// something to rustdoc and must be unwrapped to inline code.
fn is_rust_path_target(target: &str) -> bool {
    !target.is_empty()
        && target.split("::").all(|seg| {
            !seg.is_empty() && seg.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
}

#[cfg(test)]
mod tests {
    use super::unwrap_rustdoc_links;

    #[test]
    fn unwrap_rustdoc_links_strips_brackets_and_self_prefix() {
        assert_eq!(
            unwrap_rustdoc_links("layered on top of [`Self::upload_concurrency`] (the cap)"),
            "layered on top of `upload_concurrency` (the cap)"
        );
        assert_eq!(
            unwrap_rustdoc_links("[`AttestationMode::Subjects`] emits a manifest"),
            "`AttestationMode::Subjects` emits a manifest"
        );
        // Multiple links in one description.
        assert_eq!(
            unwrap_rustdoc_links("see [`Self::a`] and [`b::c`]"),
            "see `a` and `b::c`"
        );
        // A non-`Self::` path keeps its full form.
        assert_eq!(unwrap_rustdoc_links("[`b::c`]"), "`b::c`");
        // Explicit rust-path target: the href means nothing outside rustdoc,
        // so the link collapses to its code text.
        assert_eq!(
            unwrap_rustdoc_links("see [`apply_legacy`](super::super::apply_legacy) for details"),
            "see `apply_legacy` for details"
        );
    }

    #[test]
    fn unwrap_rustdoc_links_leaves_real_markdown_links_alone() {
        assert_eq!(
            unwrap_rustdoc_links("[`docs`](https://example.com) and [`x`][ref]"),
            "[`docs`](https://example.com) and [`x`][ref]"
        );
        // No link syntax at all: pass through unchanged.
        assert_eq!(
            unwrap_rustdoc_links("plain `code` text"),
            "plain `code` text"
        );
        // Unterminated bracket: pass through unchanged.
        assert_eq!(unwrap_rustdoc_links("broken [`link"), "broken [`link");
    }

    #[test]
    fn unwrap_rustdoc_links_reads_no_bracket_inside_a_code_span_as_a_link() {
        let text = "ends with `,`, `(`, `[`, `{`, `|` or `]`, and `[`a`]`";
        assert_eq!(unwrap_rustdoc_links(text), text);
        assert_eq!(
            unwrap_rustdoc_links("opened more `(`, `[` and `{` than it closed; see [`Self::x`]"),
            "opened more `(`, `[` and `{` than it closed; see `x`"
        );
        assert_eq!(
            unwrap_rustdoc_links("a ``[`x`]`` span and [`y`]"),
            "a ``[`x`]`` span and `y`"
        );
    }
}
