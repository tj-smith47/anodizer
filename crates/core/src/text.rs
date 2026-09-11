//! Plain-text shaping shared by stages that fit a string into a byte budget.

/// The marker anodizer appends where it cut an over-long string.
/// GoReleaser's is the same three-dot ellipsis.
pub const TRUNCATION_ELLIPSIS: &str = "...";

/// Cut `s` down to at most `max_len` bytes, ending it with
/// [`TRUNCATION_ELLIPSIS`], never splitting a UTF-8 character.
///
/// The budget is the whole result, marker included, because the callers'
/// limits (a GitHub release body, a drift summary line) are limits on what
/// they emit. A `max_len` too small to hold even the marker yields an empty
/// string — nothing of the original survives, and a partial marker would read
/// as content.
pub fn truncate_with_ellipsis(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    let Some(max_content) = max_len.checked_sub(TRUNCATION_ELLIPSIS.len()) else {
        return String::new();
    };
    let safe_end = s
        .char_indices()
        .map(|(i, c)| i + c.len_utf8())
        .take_while(|&end| end <= max_content)
        .last()
        .unwrap_or(0);
    format!("{}{}", &s[..safe_end], TRUNCATION_ELLIPSIS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_string_inside_the_budget_is_returned_whole() {
        assert_eq!(truncate_with_ellipsis("abcdef", 6), "abcdef");
    }

    #[test]
    fn the_marker_counts_against_the_budget() {
        assert_eq!(truncate_with_ellipsis("abcdef", 5), "ab...");
        assert_eq!(truncate_with_ellipsis("abcdef", 5).len(), 5);
    }

    #[test]
    fn a_multibyte_character_is_never_split() {
        // "é" is two bytes, so a 6-byte string under a 5-byte budget leaves
        // room for two content bytes: one whole character, never half of the
        // second.
        let out = truncate_with_ellipsis("ééé", 5);
        assert_eq!(out, "é...");
        assert!(out.len() <= 5);
    }

    #[test]
    fn a_budget_below_the_marker_yields_nothing() {
        assert_eq!(truncate_with_ellipsis("abcdef", 2), "");
    }
}
