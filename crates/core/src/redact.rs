//! Secret redaction for command output.
//!
//! Mirrors GoReleaser's `internal/redact/redact.go`: scans environment
//! variables for secret-looking entries and replaces their values in
//! output strings with `$KEY_NAME`.

/// Key suffixes that indicate a secret value.
const SECRET_KEY_SUFFIXES: &[&str] = &["_KEY", "_SECRET", "_PASSWORD", "_TOKEN"];

/// Value prefixes that indicate a secret regardless of key name.
const SECRET_VALUE_PREFIXES: &[&str] = &[
    "sk-",
    "ghp_",
    "ghs_",
    "gho_",
    "ghu_",
    "dckr_pat_",
    "glpat-",
    "AIZA",
    "xox",
];

/// Returns true if this env entry looks like it contains a secret.
///
/// The empty string is the only excluded value — every non-empty value
/// matching the heuristics is redacted, mirroring upstream
/// GoReleaser's `internal/redact/redact.go::isSecret` after the
/// length-floor was removed (commit `d1cdbb2`).
fn is_secret(key: &str, value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    let key_upper = key.to_uppercase();
    if SECRET_KEY_SUFFIXES.iter().any(|s| key_upper.ends_with(s)) {
        return true;
    }
    SECRET_VALUE_PREFIXES.iter().any(|p| value.starts_with(p))
}

/// Redact secret values in a string, replacing them with `$KEY_NAME`.
///
/// Longer values are replaced first to prevent partial matches.
///
/// Mirrors GoReleaser's `redact.String(s, env)` API.
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

/// Convenience wrapper: redact secrets in `input` using the current
/// process env (`std::env::vars()`) PLUS strip inline URL credentials.
///
/// Used by modules that don't have a `Context` in scope (e.g. the `git/`
/// shell-out helpers) and still want the same redaction surface as the
/// `StageLogger`. Equivalent to `redact_url_credentials(input)` followed
/// by `string(..., &process_env_vec)`.
pub fn redact_process_env(input: &str) -> String {
    let env: Vec<(String, String)> = std::env::vars().collect();
    let stripped = redact_url_credentials(input);
    string(&stripped, &env)
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
        // Mirrors upstream rename in `internal/redact/redact_test.go` after
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
        // an embedded credential. Mirrors `git::remote::strip_url_credentials`.
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
}
