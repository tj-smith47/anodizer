//! Residual-template-delimiter guard for generated manifests.
//!
//! After a publisher renders a manifest to its final text, that text MUST NOT
//! still contain unrendered Go/Tera `{{ … }}` template delimiters. A residual
//! delimiter means a user-supplied config string field was emitted verbatim
//! instead of being run through the template engine — the bug class this guard
//! exists to make unrepresentable (cfgd v0.4.0 shipped a Chocolatey `docs_url`
//! containing URL-encoded `{{ .Tag }}`).
//!
//! Only the `{{` … `}}` delimiter pair is scanned. The manifest formats this
//! guard protects (nuspec XML, scoop/winget/krew JSON+YAML, homebrew Ruby, nix
//! derivations, AUR PKGBUILD/.SRCINFO, snapcraft YAML) do not legitimately
//! contain that pair: Ruby string interpolation is `#{}`, nix is `${}`,
//! shell/PowerShell is `$`. So a `{{ … }}` in final text is always a leak.

use anyhow::{Result, bail};

/// A residual `{{` … `}}` delimiter pair found in finished manifest text.
///
/// Returned by [`find_unrendered`]; the `snippet` is already secret-redacted
/// (see [`assert_no_unrendered`]) so it is safe to surface in logs or errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Residual {
    /// The offending `{{ … }}` substring (redacted), bounded so a runaway
    /// open-delimiter cannot dump the whole manifest into a log line.
    pub snippet: String,
}

/// Maximum bytes of context captured for a residual snippet, so an unbalanced
/// `{{` with no closing `}}` (or a very long templated value) cannot spill an
/// unbounded amount of manifest text into a warning or error message.
const MAX_SNIPPET: usize = 120;

/// Find the first residual `{{` … `}}` delimiter pair in `text`, if any.
///
/// Returns `None` for clean manifests. The returned [`Residual::snippet`] is
/// the raw (un-redacted) matched substring — callers that surface it MUST
/// redact first; prefer [`assert_no_unrendered`], which redacts for you.
fn find_unrendered_raw(text: &str) -> Option<String> {
    let open = text.find("{{")?;
    // Bound the captured region: prefer the matching `}}`, but never exceed
    // MAX_SNIPPET so a missing close delimiter can't dump the whole manifest.
    let rest = &text[open..];
    let mut end = match rest.find("}}") {
        Some(close) => (close + 2).min(MAX_SNIPPET),
        None => rest.len().min(MAX_SNIPPET),
    };
    // `end` is a raw byte index that the MAX_SNIPPET clamp can land mid-codepoint
    // when an unbalanced `{{` is followed by multibyte content (emoji/CJK/accent)
    // — slicing there would panic. Walk back to the nearest char boundary first.
    while end > 0 && !rest.is_char_boundary(end) {
        end -= 1;
    }
    Some(rest[..end].to_string())
}

/// Assert that `text` (a publisher's finished manifest) contains no residual
/// `{{ … }}` template delimiters.
///
/// `label` names the publisher + manifest (e.g. `"chocolatey nuspec"`) for the
/// diagnostic. `redact` is applied to the offending snippet before it is
/// surfaced, so a secret-flagged config value cannot leak into a log line or
/// error message — callers pass [`crate::redact::redact_process_env`] (or an
/// equivalent that also masks config-declared secrets).
///
/// - **strict** (`is_strict == true`): returns `Err` naming the redacted
///   snippet and `label`, so a leaking manifest fails the publish BEFORE any
///   irreversible publisher fires.
/// - **non-strict**: returns `Ok(Some(Residual))` with the redacted snippet so
///   the caller can `log.warn(...)` it; returns `Ok(None)` when clean.
///
/// A clean manifest always returns `Ok(None)` regardless of `is_strict`.
pub fn assert_no_unrendered(
    text: &str,
    label: &str,
    is_strict: bool,
    redact: impl Fn(&str) -> String,
) -> Result<Option<Residual>> {
    match find_unrendered_raw(text) {
        None => Ok(None),
        Some(raw) => {
            let snippet = redact(&raw);
            if is_strict {
                bail!("{}", unrendered_message(label, &snippet));
            }
            Ok(Some(Residual { snippet }))
        }
    }
}

/// [`assert_no_unrendered`] plus the lenient-mode warn emission, so the
/// warn-message wording lives in exactly one place across every caller.
///
/// Strict mode bails (via [`assert_no_unrendered`]). Lenient mode invokes
/// `warn` with the fully-formatted line — same text as the strict error body —
/// describing the redacted residual snippet. The `warn` closure lets callers
/// route through their own `StageLogger` (which `anodizer-core` cannot depend
/// on at this layer without a cycle) while keeping the message canonical.
///
/// `label` and `redact` carry the same meaning as in [`assert_no_unrendered`].
pub fn assert_no_unrendered_logged(
    text: &str,
    label: &str,
    is_strict: bool,
    redact: impl Fn(&str) -> String,
    warn: impl FnOnce(&str),
) -> Result<()> {
    if let Some(residual) = assert_no_unrendered(text, label, is_strict, redact)? {
        warn(&unrendered_message(label, &residual.snippet));
    }
    Ok(())
}

/// The single canonical phrasing for a residual-delimiter diagnostic, shared by
/// the strict-bail path and the lenient-warn path so the two never drift.
fn unrendered_message(label: &str, snippet: &str) -> String {
    format!(
        "{label}: unrendered template delimiter in generated manifest: {snippet:?} \
         (a user-supplied config field was emitted without template rendering)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(s: &str) -> String {
        s.to_string()
    }

    #[test]
    fn clean_manifest_passes_strict_and_lenient() {
        let clean = "url = \"https://example.com/v1.2.3/tool.tar.gz\"\nsha256 = \"abc\"";
        assert_eq!(
            assert_no_unrendered(clean, "test", true, identity).unwrap(),
            None
        );
        assert_eq!(
            assert_no_unrendered(clean, "test", false, identity).unwrap(),
            None
        );
    }

    #[test]
    fn ruby_and_nix_interpolation_do_not_false_positive() {
        // Ruby `#{}` and nix `${}` are legitimate manifest syntax.
        let ruby = "depends_on macos: :catalina\ncaveats { \"installed #{version}\" }";
        let nix = "src = fetchurl { url = \"${baseUrl}/tool\"; };";
        assert_eq!(
            assert_no_unrendered(ruby, "homebrew", true, identity).unwrap(),
            None
        );
        assert_eq!(
            assert_no_unrendered(nix, "nix", true, identity).unwrap(),
            None
        );
    }

    #[test]
    fn leaked_tag_fails_in_strict_mode() {
        let leaked = "docs_url = \"https://x/y/blob/{{ .Tag }}/docs.md\"";
        let err = assert_no_unrendered(leaked, "chocolatey nuspec", true, identity).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("chocolatey nuspec"), "label missing: {msg}");
        assert!(msg.contains("{{ .Tag }}"), "snippet missing: {msg}");
    }

    #[test]
    fn leaked_tag_warns_in_lenient_mode() {
        let leaked = "url=\"https://x/{{ .Version }}/t\"";
        let residual = assert_no_unrendered(leaked, "aur PKGBUILD", false, identity)
            .unwrap()
            .expect("expected a residual");
        assert!(residual.snippet.contains("{{ .Version }}"));
    }

    #[test]
    fn snippet_is_redacted_before_surfacing() {
        let leaked = "token = {{ ghp_supersecrettoken123 }}";
        let redactor = |s: &str| s.replace("ghp_supersecrettoken123", "$REDACTED");
        // Strict: secret must not appear in the error.
        let err = assert_no_unrendered(leaked, "x", true, redactor).unwrap_err();
        assert!(!err.to_string().contains("ghp_supersecrettoken123"));
        assert!(err.to_string().contains("$REDACTED"));
        // Lenient: secret must not appear in the residual snippet.
        let residual = assert_no_unrendered(leaked, "x", false, redactor)
            .unwrap()
            .unwrap();
        assert!(!residual.snippet.contains("ghp_supersecrettoken123"));
        assert!(residual.snippet.contains("$REDACTED"));
    }

    #[test]
    fn unbalanced_open_delimiter_is_bounded() {
        let runaway = format!("{{{{ {}", "A".repeat(500));
        let residual = assert_no_unrendered(&runaway, "x", false, identity)
            .unwrap()
            .unwrap();
        assert!(residual.snippet.len() <= MAX_SNIPPET, "snippet not bounded");
    }

    #[test]
    fn unbalanced_open_delimiter_with_multibyte_does_not_panic() {
        // An unbalanced `{{` followed by multibyte content straddling the
        // MAX_SNIPPET byte clamp must not panic on a non-char-boundary slice.
        // Each "日本語" is 9 bytes; ~30 copies overruns the 120-byte clamp so the
        // cut lands mid-codepoint unless the boundary walk-back applies.
        let runaway = format!("{{{{ {}", "日本語".repeat(40));
        let residual = assert_no_unrendered(&runaway, "x", false, identity)
            .unwrap()
            .unwrap();
        assert!(residual.snippet.len() <= MAX_SNIPPET, "snippet not bounded");
        // Round-trips as valid UTF-8 (it is already a String, but assert the
        // boundary walk-back left no partial codepoint by re-validating bytes).
        assert!(std::str::from_utf8(residual.snippet.as_bytes()).is_ok());

        // Also exercise the no-close-delimiter emoji case at the boundary.
        let emoji = format!("{{{{ {}", "🦀".repeat(40));
        let r2 = assert_no_unrendered(&emoji, "x", false, identity)
            .unwrap()
            .unwrap();
        assert!(r2.snippet.len() <= MAX_SNIPPET);
        assert!(std::str::from_utf8(r2.snippet.as_bytes()).is_ok());
    }

    #[test]
    fn logged_helper_warns_once_in_lenient_and_bails_in_strict() {
        let leaked = "url=\"https://x/{{ .Version }}/t\"";
        // Lenient: warn fires exactly once with the canonical message.
        let mut warns: Vec<String> = Vec::new();
        assert_no_unrendered_logged(leaked, "scoop manifest", false, identity, |m| {
            warns.push(m.to_string())
        })
        .unwrap();
        assert_eq!(warns.len(), 1, "warn must fire exactly once");
        assert!(warns[0].contains("scoop manifest"));
        assert!(warns[0].contains("{{ .Version }}"));
        assert!(warns[0].contains("unrendered template delimiter"));

        // Strict: bails, warn never fires.
        let mut strict_warns = 0usize;
        let err = assert_no_unrendered_logged(leaked, "scoop manifest", true, identity, |_| {
            strict_warns += 1
        })
        .unwrap_err();
        assert_eq!(strict_warns, 0, "strict path must not warn");
        // Strict error body and lenient warn body share the same wording.
        assert!(err.to_string().contains("unrendered template delimiter"));
        assert!(err.to_string().contains("scoop manifest"));

        // Clean text: no warn, Ok.
        let mut clean_warns = 0usize;
        assert_no_unrendered_logged("clean", "x", false, identity, |_| clean_warns += 1).unwrap();
        assert_eq!(clean_warns, 0);
    }
}
