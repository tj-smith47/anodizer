//! Shields bash `${#…}` parameter-length expansions from Tera rendering.
//!
//! Tera reserves `{#` as a comment-open (paired with `#}`), so a bash
//! `${#arr[@]}` makes Tera expect a comment terminator that never arrives and
//! the render fails. Go `text/template` (what GoReleaser uses) spells comments
//! `{{/* */}}` and has no such collision, so bash `${#…}` flows through it
//! untouched. To mirror that behavior, `${#` is swapped for an inert sentinel
//! before the Tera pass and restored afterward, leaving the expansion literal.
//!
//! Only the `${#` trigraph is targeted: the leading `$` makes it unambiguously
//! bash, so a standalone `{# … #}` Tera comment still renders normally.

/// Private-use-area sentinel that replaces `${#` across the Tera render.
///
/// Contains none of `{{`, `{%`, or `{#`, so it is inert to Tera's parser and to
/// the env-var scan that runs on the protected string. A single shared constant
/// keeps [`protect_shell_param_length`] and [`restore_shell_param_length`] from
/// drifting.
const SHELL_PARAMLEN_SENTINEL: &str = "\u{E000}ANODIZER_SHELL_PARAMLEN\u{E000}";

/// Replace every literal `${#` with the inert [`SHELL_PARAMLEN_SENTINEL`] so
/// bash parameter-length syntax survives the Tera render verbatim.
///
/// The inverse of [`restore_shell_param_length`]. Borrows the input unchanged
/// when no `${#` is present (the common case). Errors if the input already
/// contains the sentinel, since the inverse restore could not then tell a
/// protected `${#` apart from a literal sentinel and would silently corrupt it.
pub(crate) fn protect_shell_param_length(s: &str) -> anyhow::Result<std::borrow::Cow<'_, str>> {
    if s.contains(SHELL_PARAMLEN_SENTINEL) {
        anyhow::bail!("template contains the reserved shell-guard sentinel");
    }
    if s.contains("${#") {
        Ok(std::borrow::Cow::Owned(
            s.replace("${#", SHELL_PARAMLEN_SENTINEL),
        ))
    } else {
        Ok(std::borrow::Cow::Borrowed(s))
    }
}

/// Replace every [`SHELL_PARAMLEN_SENTINEL`] back with `${#`, exactly undoing
/// [`protect_shell_param_length`]. Borrows the input unchanged when no sentinel
/// is present (the common case — restore runs on every rendered string).
pub(crate) fn restore_shell_param_length(s: &str) -> std::borrow::Cow<'_, str> {
    if s.contains(SHELL_PARAMLEN_SENTINEL) {
        std::borrow::Cow::Owned(s.replace(SHELL_PARAMLEN_SENTINEL, "${#"))
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(original: &str) -> String {
        let protected = protect_shell_param_length(original).expect("protect must not error");
        restore_shell_param_length(&protected).into_owned()
    }

    #[test]
    fn protect_replaces_dollar_brace_hash() {
        let protected = protect_shell_param_length("n=${#f[@]}").unwrap();
        assert!(!protected.contains("${#"));
        assert!(!protected.contains("{#"));
    }

    #[test]
    fn round_trip_is_exact() {
        let original = "(( ${#f[@]} )) || exit 1; m=${#g[*]}";
        assert_eq!(round_trip(original), original);
    }

    #[test]
    fn bare_tera_comment_open_is_untouched() {
        let protected = protect_shell_param_length("pre {# c #} post").unwrap();
        assert_eq!(protected, "pre {# c #} post");
    }

    #[test]
    fn dollar_brace_hash_at_start_round_trips() {
        assert_eq!(round_trip("${#x}"), "${#x}");
    }

    #[test]
    fn dollar_brace_hash_at_end_round_trips() {
        assert_eq!(round_trip("a=${#"), "a=${#");
    }

    #[test]
    fn back_to_back_dollar_brace_hash_round_trips() {
        let protected = protect_shell_param_length("${#${#").unwrap();
        let sentinels = protected.matches(SHELL_PARAMLEN_SENTINEL).count();
        assert_eq!(sentinels, 2);
        assert_eq!(
            restore_shell_param_length(&protected).into_owned(),
            "${#${#"
        );
    }

    #[test]
    fn pre_existing_sentinel_is_rejected() {
        let poisoned = format!("echo {SHELL_PARAMLEN_SENTINEL}");
        assert!(protect_shell_param_length(&poisoned).is_err());
    }
}
