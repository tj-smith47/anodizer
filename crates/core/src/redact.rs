//! Secret redaction for command output.
//!
//! Scans environment
//! variables for secret-looking entries and replaces their values in
//! output strings with `$KEY_NAME`.

/// Key suffixes that indicate a secret value.
///
/// `_KEY` covers AI provider API keys (`ANTHROPIC_API_KEY`,
/// `OPENAI_API_KEY`) alongside signing-key and other historical
/// secret-bearing variable names.
pub const SECRET_KEY_SUFFIXES: &[&str] = &["_KEY", "_SECRET", "_PASSWORD", "_TOKEN"];

/// Value prefixes that indicate a secret regardless of key name.
///
/// Catches provider API keys (`sk-...`, `sk-ant-...`) regardless of the
/// variable name they happen to be exported under. Matching is
/// case-sensitive, so prefixes must reproduce the issuer's exact casing
/// (`AIza`, not `AIZA`).
const SECRET_VALUE_PREFIXES: &[&str] = &[
    "sk-",
    "ghp_",
    "ghs_",
    "gho_",
    "ghu_",
    "github_pat_",
    "dckr_pat_",
    "glpat-",
    "AIza",
    "ya29.",
    "xox",
];

/// Returns true if this env entry looks like it contains a secret.
///
/// The empty string is the only excluded value — every non-empty value
/// matching the heuristics is redacted, mirroring upstream
/// Secret-detection heuristic after the
/// length-floor was removed (commit `d1cdbb2`).
fn is_secret(key: &str, value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    let key_upper = key.to_uppercase();
    if !is_boolean_shape(value) && SECRET_KEY_SUFFIXES.iter().any(|s| key_upper.ends_with(s)) {
        return true;
    }
    SECRET_VALUE_PREFIXES.iter().any(|p| value.starts_with(p))
}

/// A value that shares a `*_TOKEN`/`*_KEY`/`*_SECRET`/`*_PASSWORD` key name
/// but cannot be a credential by its shape: a boolean literal used as an
/// enable-flag. Covers both the word forms (`USE_TOKEN=true`,
/// `SIGN_WITH_KEY=false`) and the canonical numeric flags `0`/`1`
/// (`HF_HUB_DISABLE_IMPLICIT_TOKEN=1`, `GPG_SIGN_KEY=0`) — the single most
/// common env enable-flag idiom, where the `_TOKEN`/`_KEY` suffix names the
/// feature being toggled, not a credential.
///
/// The numeric exemption is deliberately restricted to the single characters
/// `0` and `1`: a lone bit cannot be a credential, and redacting it corrupts
/// every unrelated `0`/`1` in the same output (a version like `1.0.0`, a
/// revision number, a timestamp). A *multi*-digit numeric value (a 4-digit
/// HSM/smartcard PIN, a 6-digit OTP) is still NOT exempted, because it is
/// indistinguishable from a genuine short numeric credential and a false
/// exemption there is silent data exposure. Short *alphabetic* values are
/// likewise never exempted: a 5-char `API_KEY=short` is still masked. A
/// [`SECRET_VALUE_PREFIXES`] match takes precedence, so a positively
/// secret-shaped value is never dropped by this filter.
fn is_boolean_shape(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "true" | "false" | "yes" | "no" | "on" | "off" | "0" | "1"
    )
}

/// Redact secret values in a string, replacing them with `$KEY_NAME`.
///
/// Longer values are replaced first to prevent partial matches.
///
/// Redact secret env-var values found in a string.
pub fn string(input: &str, env: &[(String, String)]) -> String {
    let mut secrets: Vec<(&str, &str)> = env
        .iter()
        .filter(|(k, v)| is_secret(k, v))
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    secrets.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(b.0)));

    let mut result = input.to_string();
    for (key, value) in secrets {
        result = result.replace(value, &format!("${}", key));
    }
    result
}

/// Apply the full outbound-text redaction policy: strip inline URL
/// credentials, then mask known-secret env values. The single definition
/// shared by log redaction ([`crate::log::StageLogger::redact`]) and
/// announce body redaction so the two can never diverge.
pub fn with_env(input: &str, env: &[(String, String)]) -> String {
    string(&redact_url_credentials(input), env)
}

/// Convenience wrapper: redact secrets in `input` using the current
/// process env (`std::env::vars()`) PLUS strip inline URL credentials.
///
/// Used by modules that don't have a `Context` in scope (e.g. the `git/`
/// shell-out helpers) and still want the same redaction surface as the
/// `StageLogger`. Equivalent to `redact_url_credentials(input)` followed
/// by `string(..., &process_env_vec)`.
pub fn redact_process_env(input: &str) -> String {
    let env: Vec<(String, String)> = std::env::vars().collect();
    with_env(input, &env)
}

/// Strip embedded userinfo (credentials) from any URLs found in `input`.
///
/// For each occurrence of `<scheme>://<userinfo>@<host>...`, the substring
/// between `://` and the first `@` is replaced with `<redacted>`. Non-URL
/// text is left untouched, and URLs without a userinfo component are
/// unchanged. Handles `http`, `https`, and any other `<scheme>://` form.
///
/// Use this as a defense-in-depth complement to [`string`] when the secret
/// is inlined in a URL but the bare token value is not necessarily exported
/// as an env var (e.g. a `git_url` config string the user templated with a
/// literal `https://user:pass@host`).
pub fn redact_url_credentials(input: &str) -> String {
    // Walk the string and rewrite each `<scheme>://<userinfo>@` segment.
    // For each `://` we find, look up to the next path / query / fragment /
    // whitespace boundary; if that authority segment contains an `@`, the
    // text before the LAST `@` is the userinfo (RFC 3986 §3.2.1 allows
    // unreserved `@` in the password subcomponent only when percent-encoded,
    // but real-world tokens contain literal `@` often enough that we treat
    // the last `@` as the host separator).
    let mut result = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(scheme_end) = rest.find("://") {
        let after_scheme_start = scheme_end + 3;
        result.push_str(&rest[..after_scheme_start]);
        let after_scheme = &rest[after_scheme_start..];
        let terminator = after_scheme
            .find(|c: char| matches!(c, '/' | '?' | '#') || c.is_whitespace())
            .unwrap_or(after_scheme.len());
        let authority = &after_scheme[..terminator];
        if let Some(last_at) = authority.rfind('@') {
            // userinfo = authority[..last_at], host-start = last_at + 1
            result.push_str("<redacted>@");
            result.push_str(&authority[last_at + 1..]);
            rest = &after_scheme[terminator..];
        } else {
            result.push_str(authority);
            rest = &after_scheme[terminator..];
        }
    }
    result.push_str(rest);
    result
}

/// Strip bearer / authorization tokens that may have been echoed by a
/// remote endpoint into a response body before that body lands in an
/// error message. Defense in depth — if a misbehaving registry mirrors
/// the request's `Authorization` header back in an error response, this
/// helper prevents the token from showing up in user-visible logs.
///
/// Replaces:
///   - `Bearer <token>` → `Bearer <redacted>` (case-insensitive on the
///     keyword; the canonical replacement spelling is always "Bearer").
///     A "Bearer" match requires the keyword to appear at the start of
///     the input OR immediately after one of `[ \t:,;("'<\n\r]` so that
///     prose words like "bearer of bad news" do not match.
///   - `Basic <b64>` → `Basic <redacted>` (case-insensitive on the
///     keyword; same boundary rule as `Bearer`). Covers HTTP Basic
///     auth headers like the GemFury push token (`Authorization:
///     Basic <token-as-username:>` base64).
///   - `Authorization:` followed by any value through end-of-line →
///     `Authorization: <redacted>` (case-insensitive on the header name).
///     The entire header value is consumed so `Authorization: Bearer X`
///     doesn't leak `X` after the header redaction.
///
/// Use as a wrapper around any remote-supplied body text being interpolated
/// into an error message or log line. The bare token (no scheme prefix)
/// remains untouched — for that, rely on `string(..., env)` matching the
/// env-var-based heuristics.
pub fn redact_bearer_tokens(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        // Authorization: <rest-of-line>
        // Always allowed to match at i (the header name itself is unambiguous
        // when followed by a `:`). Consume through the next \n / \r so a
        // multi-line body with subsequent normal text isn't redacted past
        // the header's terminator.
        if let Some(name_len) = match_authorization_prefix(&bytes[i..]) {
            out.push_str("Authorization: <redacted>");
            i += name_len;
            while i < bytes.len() && bytes[i] != b'\n' && bytes[i] != b'\r' {
                i += 1;
            }
            continue;
        }
        // Bearer <token>
        // Require the preceding byte (if any) to be a token-boundary
        // character so prose like "the bearer of bad news" doesn't match.
        let preceded_by_boundary = i == 0
            || matches!(
                bytes[i - 1],
                b' ' | b'\t' | b':' | b',' | b';' | b'(' | b'"' | b'\'' | b'<' | b'\n' | b'\r'
            );
        if preceded_by_boundary && let Some(kw_len) = match_bearer_prefix(&bytes[i..]) {
            out.push_str("Bearer <redacted>");
            i += kw_len;
            // Skip the token value: a run of non-whitespace characters.
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            continue;
        }
        if preceded_by_boundary && let Some(kw_len) = match_basic_prefix(&bytes[i..]) {
            out.push_str("Basic <redacted>");
            i += kw_len;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            continue;
        }
        // Emit one byte verbatim and advance.
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Returns Some(prefix_len) if `bytes` starts with case-insensitive
/// "Bearer " (the trailing space is required so we don't match "Bearertown").
fn match_bearer_prefix(bytes: &[u8]) -> Option<usize> {
    const KW: &[u8] = b"Bearer ";
    if bytes.len() < KW.len() {
        return None;
    }
    for (i, kw_byte) in KW.iter().enumerate() {
        if !bytes[i].eq_ignore_ascii_case(kw_byte) {
            return None;
        }
    }
    Some(KW.len())
}

/// Returns Some(prefix_len) if `bytes` starts with case-insensitive
/// "Basic " (the trailing space is required so we don't match "Basics" or
/// "Basically"). Covers HTTP Basic auth headers used by GemFury and other
/// publishers that pass the token as the Basic-auth username.
fn match_basic_prefix(bytes: &[u8]) -> Option<usize> {
    const KW: &[u8] = b"Basic ";
    if bytes.len() < KW.len() {
        return None;
    }
    for (i, kw_byte) in KW.iter().enumerate() {
        if !bytes[i].eq_ignore_ascii_case(kw_byte) {
            return None;
        }
    }
    Some(KW.len())
}

/// Returns Some(prefix_len) if `bytes` starts with case-insensitive
/// "Authorization:" (the trailing colon is required to disambiguate from
/// prose mentioning the word "authorization").
fn match_authorization_prefix(bytes: &[u8]) -> Option<usize> {
    const KW: &[u8] = b"Authorization:";
    if bytes.len() < KW.len() {
        return None;
    }
    for (i, kw_byte) in KW.iter().enumerate() {
        if !bytes[i].eq_ignore_ascii_case(kw_byte) {
            return None;
        }
    }
    Some(KW.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_redact_by_key_suffix() {
        let env = vec![
            (
                "DOCKER_PASSWORD".to_string(),
                "mysecretpassword123".to_string(),
            ),
            ("PLAIN_VAR".to_string(), "not-a-secret".to_string()),
        ];
        let result = string("Login with mysecretpassword123 succeeded", &env);
        assert_eq!(result, "Login with $DOCKER_PASSWORD succeeded");
        assert!(!result.contains("mysecretpassword123"));
    }

    #[test]
    fn test_redact_by_value_prefix() {
        let env = vec![("MY_TOKEN".to_string(), "ghp_abc123def456ghi789".to_string())];
        let result = string("Using token ghp_abc123def456ghi789", &env);
        assert_eq!(result, "Using token $MY_TOKEN");
    }

    #[test]
    fn test_redact_includes_short_secret_when_key_looks_secret() {
        // Reflects the secret-key rename after
        // the length-floor was removed: a 5-char value under a `*_KEY` key
        // must still be redacted.
        let env = vec![("API_KEY".to_string(), "short".to_string())];
        let result = string("Value is short", &env);
        assert_eq!(result, "Value is $API_KEY");
    }

    #[test]
    fn test_redact_skips_empty_value() {
        // The empty string is the only excluded value: an unset env var
        // would otherwise replace every empty substring in the input,
        // turning "abc" into "$API_KEY a$API_KEY b$API_KEY c$API_KEY".
        let env = vec![("API_KEY".to_string(), String::new())];
        let result = string("Value is short", &env);
        assert_eq!(result, "Value is short");
    }

    #[test]
    fn test_redact_longer_values_first() {
        let env = vec![
            ("SHORT_TOKEN".to_string(), "abcdefghij".to_string()),
            ("LONG_TOKEN".to_string(), "abcdefghijklmnop".to_string()),
        ];
        let result = string("secret: abcdefghijklmnop", &env);
        // Longer match should be replaced first
        assert_eq!(result, "secret: $LONG_TOKEN");
    }

    #[test]
    fn test_redact_no_secrets() {
        let env = vec![("PATH".to_string(), "/usr/bin:/usr/local/bin".to_string())];
        let result = string("PATH is set", &env);
        assert_eq!(result, "PATH is set");
    }

    #[test]
    fn test_redact_multiple_occurrences() {
        let env = vec![(
            "REGISTRY_PASSWORD".to_string(),
            "supersecret123".to_string(),
        )];
        let result = string("auth supersecret123 retry supersecret123", &env);
        assert_eq!(result, "auth $REGISTRY_PASSWORD retry $REGISTRY_PASSWORD");
    }

    #[test]
    fn test_is_secret_key_suffixes() {
        assert!(is_secret("DOCKER_PASSWORD", "longvalue1234"));
        assert!(is_secret("API_TOKEN", "longvalue1234"));
        assert!(is_secret("signing_key", "longvalue1234")); // case insensitive
        assert!(is_secret("MY_SECRET", "longvalue1234"));
        assert!(!is_secret("MY_CONFIG", "longvalue1234"));
    }

    #[test]
    fn test_is_secret_value_prefixes() {
        assert!(is_secret("ANYTHING", "ghp_1234567890"));
        assert!(is_secret("ANYTHING", "sk-1234567890"));
        assert!(is_secret("ANYTHING", "dckr_pat_1234567890"));
        assert!(is_secret("ANYTHING", "glpat-1234567890"));
        // Fine-grained GitHub PAT and Google API/OAuth keys, exported under a
        // name the suffix list does not catch, are matched by exact-case value
        // prefix. `AIza`/`ya29.` casing is load-bearing: the match is
        // case-sensitive, so an `AIZA` prefix would catch zero real keys.
        assert!(is_secret("GH_PAT", "github_pat_11ABCDE0000000000"));
        assert!(is_secret(
            "GOOGLE_CREDS",
            "AIzaSyA00000000000000000000000000000000"
        ));
        assert!(is_secret(
            "GOOGLE_CREDS",
            "ya29.a0Af00000000000000000000000"
        ));
        assert!(!is_secret("ANYTHING", "regular_value1234"));
    }

    #[test]
    fn test_redact_sort_stability_same_length() {
        // When two secrets have the same value length, sort by key name
        // for deterministic output regardless of HashMap iteration order.
        let env = vec![
            ("B_SECRET".to_string(), "same_length_val".to_string()),
            ("A_SECRET".to_string(), "same_length_val".to_string()),
        ];
        // Both keys map to the same value, so whichever sorts first by
        // key name should win — A_SECRET comes before B_SECRET.
        let result = string("found same_length_val here", &env);
        assert_eq!(result, "found $A_SECRET here");
    }

    #[test]
    fn test_redact_deterministic_with_different_lengths() {
        // Longer values still replaced first, secondary sort by key is tiebreaker
        let env = vec![
            ("Z_TOKEN".to_string(), "short_secret_val".to_string()),
            (
                "A_TOKEN".to_string(),
                "a_longer_secret_value_here".to_string(),
            ),
        ];
        let result = string("prefix a_longer_secret_value_here suffix", &env);
        assert_eq!(result, "prefix $A_TOKEN suffix");
    }

    #[test]
    fn test_with_env_composes_url_strip_and_env_mask() {
        // The canonical outbound policy must apply BOTH layers in one call:
        // inline URL-credential stripping AND known-secret env masking. A body
        // carrying both must come out clean on both axes — proving composition,
        // not either layer alone.
        let env = vec![(
            "CARGO_REGISTRY_TOKEN".to_string(),
            "ghp_realsecretvalue".to_string(),
        )];
        let input = "pushed via https://tok@host/x then logged ghp_realsecretvalue";
        let result = with_env(input, &env);
        assert_eq!(
            result,
            "pushed via https://<redacted>@host/x then logged $CARGO_REGISTRY_TOKEN"
        );
        assert!(!result.contains("ghp_realsecretvalue"));
        assert!(!result.contains("tok@host"));
    }

    #[test]
    fn test_redact_url_credentials_https_with_token() {
        let input = "remote: https://ghp_abc123def@github.com/owner/repo.git";
        let result = redact_url_credentials(input);
        assert_eq!(
            result,
            "remote: https://<redacted>@github.com/owner/repo.git"
        );
        assert!(!result.contains("ghp_abc123def"));
    }

    #[test]
    fn test_redact_url_credentials_user_pass_pair() {
        let input = "pushing to https://user:p@ssw0rd@gitlab.example.com/foo/bar";
        let result = redact_url_credentials(input);
        assert_eq!(
            result, "pushing to https://<redacted>@gitlab.example.com/foo/bar",
            "userinfo must cover the entire user:pass segment up to the host-@"
        );
    }

    #[test]
    fn test_redact_url_credentials_no_userinfo_unchanged() {
        let input = "fetching https://github.com/owner/repo.git";
        assert_eq!(redact_url_credentials(input), input);
    }

    #[test]
    fn test_redact_url_credentials_ssh_unchanged() {
        // SSH-style `git@github.com:owner/repo.git` has no `://`, so the
        // helper leaves it alone. The `git@` is part of the SSH user, not
        // an embedded credential.
        let input = "fetching git@github.com:owner/repo.git";
        assert_eq!(redact_url_credentials(input), input);
    }

    #[test]
    fn test_redact_url_credentials_multiple_urls_in_one_line() {
        let input = "from https://token1@a.com/x to https://token2@b.com/y";
        let result = redact_url_credentials(input);
        assert_eq!(
            result, "from https://<redacted>@a.com/x to https://<redacted>@b.com/y",
            "both URLs must be redacted, leaving the connecting prose intact"
        );
    }

    #[test]
    fn test_redact_url_credentials_does_not_consume_path_at_sign() {
        // `@` in a path segment (after the first `/`) must NOT be treated
        // as a userinfo terminator.
        let input = "GET https://api.example.com/users/foo@bar.com/profile";
        assert_eq!(
            redact_url_credentials(input),
            input,
            "an `@` after the first `/` is part of the path, not userinfo"
        );
    }

    #[test]
    fn test_redact_url_credentials_empty_input() {
        assert_eq!(redact_url_credentials(""), "");
    }

    #[test]
    fn test_redact_url_credentials_plain_text() {
        let input = "no URLs here, just words";
        assert_eq!(redact_url_credentials(input), input);
    }

    #[test]
    fn test_redact_url_credentials_percent_encoded_userinfo() {
        // A percent-encoded `@` in the userinfo (e.g. an account name like
        // `user@name`) does not break the terminator scan: the function
        // looks for the LAST `@` before the path / query / fragment /
        // whitespace boundary, so both `@`s collapse into a single
        // `<redacted>` replacement.
        let input = "https://user%40name:pass@host.example.com/path";
        let result = redact_url_credentials(input);
        assert_eq!(result, "https://<redacted>@host.example.com/path");
        assert!(!result.contains("user%40name"));
        assert!(!result.contains("pass"));
    }

    #[test]
    fn test_redact_url_credentials_trailing_query() {
        // A `?` after the host begins the query string; userinfo must still
        // be stripped, and the query is preserved verbatim.
        let input = "https://user:pass@host.example.com?foo=bar";
        let result = redact_url_credentials(input);
        assert_eq!(result, "https://<redacted>@host.example.com?foo=bar");
        assert!(!result.contains("user:pass"));
        assert!(result.ends_with("?foo=bar"));
    }

    #[test]
    fn test_redact_url_credentials_trailing_fragment() {
        // A `#` after the host begins the fragment; userinfo must still
        // be stripped, and the fragment is preserved verbatim.
        let input = "https://user:pass@host.example.com#frag";
        let result = redact_url_credentials(input);
        assert_eq!(result, "https://<redacted>@host.example.com#frag");
        assert!(!result.contains("user:pass"));
        assert!(result.ends_with("#frag"));
    }

    #[test]
    fn test_redact_url_credentials_whitespace_boundary() {
        // Whitespace following the host terminates the authority. The
        // userinfo is redacted and the trailing prose is preserved.
        let input = "https://user:pass@host.example.com then more";
        let result = redact_url_credentials(input);
        assert_eq!(result, "https://<redacted>@host.example.com then more");
        assert!(!result.contains("user:pass"));
        assert!(result.ends_with(" then more"));
    }

    #[test]
    fn test_redact_bearer_tokens_basic() {
        let input = "auth header: Bearer ghp_abcdef123456 expires soon";
        let result = redact_bearer_tokens(input);
        assert_eq!(result, "auth header: Bearer <redacted> expires soon");
        assert!(!result.contains("ghp_abcdef123456"));
    }

    #[test]
    fn test_redact_bearer_tokens_case_insensitive() {
        // The keyword "Bearer" is case-insensitive but the canonical
        // output form is always "Bearer".
        let input = "bearer ghp_lowercase_token";
        assert_eq!(
            redact_bearer_tokens(input),
            "Bearer <redacted>",
            "lowercase 'bearer' must still redact"
        );
        let input = "BEARER ghp_uppercase_token";
        assert_eq!(redact_bearer_tokens(input), "Bearer <redacted>");
    }

    #[test]
    fn test_redact_bearer_tokens_authorization_header() {
        // "Authorization:" consumes through end-of-line, so the entire
        // header value is redacted as one unit. Trailing content after
        // a newline is preserved verbatim.
        let input = "request: Authorization: Bearer ghp_xyz\nresponse: 401";
        let result = redact_bearer_tokens(input);
        assert_eq!(
            result, "request: Authorization: <redacted>\nresponse: 401",
            "header value (including the inner Bearer token) must be redacted as one"
        );
        assert!(!result.contains("ghp_xyz"));
    }

    #[test]
    fn test_redact_bearer_tokens_authorization_header_single_line() {
        // No newline → the header value runs to end-of-input; that's fine,
        // the entire tail is redacted (defensive: better one over-redaction
        // than one leaked token).
        let input = "Authorization: Bearer ghp_xyz";
        let result = redact_bearer_tokens(input);
        assert_eq!(result, "Authorization: <redacted>");
        assert!(!result.contains("ghp_xyz"));
    }

    #[test]
    fn test_redact_bearer_tokens_no_match_unchanged() {
        // No "Bearer " / "Authorization:" tokens → string unchanged.
        // Note: we cannot distinguish prose use of "bearer" from a real
        // header; the redactor errs on the side of over-redaction (it
        // would treat "bearer of bad news" as "Bearer <redacted> bad
        // news"). Both branches are still safer than leaking a token.
        let input = "some random text with no relevant tokens here";
        assert_eq!(redact_bearer_tokens(input), input);
    }

    #[test]
    fn test_redact_bearer_tokens_over_redacts_prose_use() {
        // Documents the known over-redaction behavior: "bearer of bad
        // news" looks like a Bearer-token construct because the redactor
        // can't tell prose from a header. The trade-off is intentional —
        // safer to over-redact a prose word than to leak a real token.
        let input = "the bearer of bad news arrived";
        let result = redact_bearer_tokens(input);
        assert_eq!(result, "the Bearer <redacted> bad news arrived");
    }

    #[test]
    fn test_redact_bearer_tokens_empty_input() {
        assert_eq!(redact_bearer_tokens(""), "");
    }

    #[test]
    fn test_redact_basic_token_redacts_b64_payload() {
        let input = "auth: Basic ZnVyeXRva2VuOg== rest";
        let result = redact_bearer_tokens(input);
        assert_eq!(result, "auth: Basic <redacted> rest");
        assert!(!result.contains("ZnVyeXRva2VuOg=="));
    }

    #[test]
    fn test_redact_basic_token_case_insensitive() {
        let input = "auth: basic ZnVyeXRva2VuOg==";
        assert_eq!(redact_bearer_tokens(input), "auth: Basic <redacted>");
    }

    #[test]
    fn test_redact_bearer_tokens_handles_multiple_occurrences() {
        let input = "first Bearer ghp_aaa and second Bearer ghp_bbb done";
        let result = redact_bearer_tokens(input);
        assert_eq!(
            result,
            "first Bearer <redacted> and second Bearer <redacted> done"
        );
        assert!(!result.contains("ghp_aaa"));
        assert!(!result.contains("ghp_bbb"));
    }

    #[test]
    fn numeric_boolean_flag_does_not_corrupt_http_status_regression() {
        // A `*_TOKEN`-suffixed var set to the enable-flag `1` must NOT rewrite
        // every `1` in unrelated text — `HTTP 401` must stay `HTTP 401`, not
        // become `HTTP 40$…`. A single bit carries no cryptographic value, so
        // it cannot be a credential worth protecting; masking it only corrupts
        // output and breaks downstream parses. Multi-digit numeric secrets (a
        // 4-digit PIN, a 6-digit OTP) remain masked — see
        // `short_numeric_pin_under_password_key_is_masked` and
        // `six_digit_otp_under_token_key_is_masked`.
        let env = vec![("HF_HUB_DISABLE_IMPLICIT_TOKEN".to_string(), "1".to_string())];
        assert_eq!(
            string("HTTP 401 unauthorized", &env),
            "HTTP 401 unauthorized"
        );
    }

    #[test]
    fn boolean_flag_under_secret_key_name_is_not_masked() {
        let env = vec![("FEATURE_TOKEN".to_string(), "true".to_string())];
        assert_eq!(string("enabled=true here", &env), "enabled=true here");
    }

    #[test]
    fn short_alphabetic_secret_under_secret_key_name_is_still_masked() {
        // The deliberately-kept behavior: a short *alphabetic* value under a
        // secret key name is a credential and stays masked.
        let env = vec![("API_KEY".to_string(), "short".to_string())];
        assert_eq!(string("Value is short", &env), "Value is $API_KEY");
    }

    #[test]
    fn numeric_value_matching_prefix_rule_is_still_masked() {
        // A positively secret-shaped value is masked even if numeric-adjacent;
        // the shape filter only guards the key-name branch.
        let env = vec![("CI".to_string(), "ghp_1".to_string())];
        assert_eq!(string("token ghp_1", &env), "token $CI");
    }

    #[test]
    fn long_numeric_value_under_secret_key_is_masked() {
        // Only the single-character boolean flags `0`/`1` are digit-exempt
        // under a secret-suffixed key (see `is_boolean_shape`) — a 32-digit
        // token under a `*_TOKEN` key must be redacted, same as any other
        // multi-digit length.
        let value = "1".repeat(32);
        let env = vec![("MY_TOKEN".to_string(), value.clone())];
        let result = string(&format!("token {value}"), &env);
        assert_eq!(result, "token $MY_TOKEN");
        assert!(!result.contains(&value));
    }

    #[test]
    fn six_digit_otp_under_token_key_is_masked() {
        // A 6-digit OTP/PIN (e.g. a PyPI 2FA code) under a `*_TOKEN` key is a
        // real, sensitive value; no digit-shape exemption applies.
        let env = vec![("PYPI_2FA_TOKEN".to_string(), "483920".to_string())];
        let result = string("submitting code 483920", &env);
        assert_eq!(result, "submitting code $PYPI_2FA_TOKEN");
        assert!(!result.contains("483920"));
    }

    #[test]
    fn short_numeric_pin_under_password_key_is_masked() {
        // A 4-digit HSM/smartcard PIN under a `*_PASSWORD` key is a real
        // credential — the short-digit exemption must never apply to a
        // secret-suffixed key, no matter how short the value is.
        let env = vec![("CARD_PASSWORD".to_string(), "1234".to_string())];
        let result = string("PIN entered: 1234", &env);
        assert_eq!(result, "PIN entered: $CARD_PASSWORD");
        assert!(!result.contains("1234"));
    }

    #[test]
    fn short_numeric_pin_under_key_suffix_is_masked() {
        let env = vec![("SIGNING_KEY".to_string(), "9999".to_string())];
        let result = string("key value 9999", &env);
        assert_eq!(result, "key value $SIGNING_KEY");
        assert!(!result.contains("9999"));
    }

    #[test]
    fn boolean_literal_under_secret_key_name_stays_unmasked_regression() {
        // The boolean-literal exemption survives the short-digit-exemption
        // removal: a `*_TOKEN`-named var used as an enable-flag must not
        // rewrite every "true" in unrelated output.
        let env = vec![("ENABLE_TOKEN".to_string(), "true".to_string())];
        assert_eq!(string("flag is true", &env), "flag is true");
    }

    #[test]
    fn non_secret_suffixed_key_short_number_stays_unmasked_regression() {
        // A key with no secret suffix never enters the key-suffix masking
        // branch at all; a small count/id value must stay untouched.
        let env = vec![("RETRY_COUNT".to_string(), "42".to_string())];
        assert_eq!(string("retries: 42", &env), "retries: 42");
    }

    #[test]
    fn numeric_boolean_flag_under_token_key_does_not_corrupt_output_regression() {
        // The real-world trap: `HF_HUB_DISABLE_IMPLICIT_TOKEN=1` names the
        // feature being toggled, not a credential. Its value `1` must NOT be
        // treated as a secret — masking a lone bit rewrites every unrelated
        // `1` in the output (a version, a revision number, a timestamp),
        // silently corrupting downstream parses such as `snapcraft
        // list-revisions` version matching.
        let env = vec![("HF_HUB_DISABLE_IMPLICIT_TOKEN".to_string(), "1".to_string())];
        let line = "7  2024-06-01T00:00:00Z  amd64  1.0.0  stable";
        assert_eq!(string(line, &env), line);
    }

    #[test]
    fn numeric_zero_flag_under_key_suffix_stays_unmasked_regression() {
        // `0` is the other canonical numeric enable-flag; it must not rewrite
        // unrelated zeros in output (`v0.9.0`, a revision `10`).
        let env = vec![("GPG_SIGN_KEY".to_string(), "0".to_string())];
        assert_eq!(string("sign=0 rev 10 v0.9.0", &env), "sign=0 rev 10 v0.9.0");
    }
}
