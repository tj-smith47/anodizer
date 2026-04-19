//! URL encoding helpers.
//!
//! `percent_encode_unreserved` implements the RFC 3986 unreserved-character
//! set used by OAuth 1.0 signatures and generic URL path segments. Prior
//! duplicates lived in `stage-announce/src/twitter.rs` and
//! `cli/src/commands/release/milestones.rs` with byte-equivalent but
//! independently-defined character sets.

use percent_encoding::{AsciiSet, CONTROLS, NON_ALPHANUMERIC, utf8_percent_encode};

/// RFC 3986 unreserved set: `A-Z a-z 0-9 - _ . ~`. Every other byte is
/// encoded as `%XX`.
const UNRESERVED: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'!')
    .add(b'"')
    .add(b'#')
    .add(b'$')
    .add(b'%')
    .add(b'&')
    .add(b'\'')
    .add(b'(')
    .add(b')')
    .add(b'*')
    .add(b'+')
    .add(b',')
    .add(b'/')
    .add(b':')
    .add(b';')
    .add(b'<')
    .add(b'=')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// Percent-encode every byte that isn't in the RFC 3986 unreserved set
/// (`A-Z a-z 0-9 - _ . ~`). Used for OAuth 1.0 signature base strings and
/// generic URL path/query segments where only unreserved chars pass through.
pub fn percent_encode_unreserved(s: &str) -> String {
    utf8_percent_encode(s, UNRESERVED).to_string()
}

/// Encode set for a single URL path segment: everything that isn't alphanumeric
/// or one of `- _ .` is percent-encoded. Notably `+`, `#`, `?`, `/`, space, and
/// all other reserved characters are encoded — safe for tag names, owner/repo
/// names, file names, and GitLab project-id path segments (where `/` must
/// become `%2F`).
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.');

/// Percent-encode a single URL path segment.
///
/// Keeps only `A-Z a-z 0-9 - _ .`. Used for tags, owner/repo names, package
/// names, versions, and file names in release backend URLs so that identifiers
/// like `v1.0.0+build.1` or `group/project` are safely encoded (`+` → `%2B`,
/// `/` → `%2F`). Unifies previously-duplicated sets in the GitHub/GitLab/Gitea
/// release backends that produced diverging URLs for the same tag.
pub fn percent_encode_path_segment(s: &str) -> String {
    utf8_percent_encode(s, PATH_SEGMENT).to_string()
}

/// Join `path` onto `base`, ensuring exactly one `/` between them.
/// Trailing `/` on `base` and a leading `/` on `path` are both tolerated.
pub fn join(base: &str, path: &str) -> String {
    let b = base.trim_end_matches('/');
    let p = path.trim_start_matches('/');
    if p.is_empty() {
        b.to_string()
    } else {
        format!("{}/{}", b, p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreserved_passes_through() {
        assert_eq!(percent_encode_unreserved("hello"), "hello");
        assert_eq!(percent_encode_unreserved("A-Za-z0-9-_.~"), "A-Za-z0-9-_.~");
    }

    #[test]
    fn space_and_specials_encoded() {
        assert_eq!(percent_encode_unreserved("hello world"), "hello%20world");
        assert_eq!(percent_encode_unreserved("a=b&c=d"), "a%3Db%26c%3Dd");
    }

    #[test]
    fn slashes_encoded() {
        assert_eq!(percent_encode_unreserved("a/b/c"), "a%2Fb%2Fc");
    }

    #[test]
    fn utf8_encoded_per_byte() {
        // é = 0xC3 0xA9 in UTF-8
        assert_eq!(percent_encode_unreserved("café"), "caf%C3%A9");
    }
}
