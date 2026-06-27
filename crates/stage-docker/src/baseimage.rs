//! Parse `FROM` directives from a Dockerfile to expose the final stage's
//! base image (and its manifest digest) as template variables.
//!
//! The two values feed the `{{ .BaseImage }}` / `{{ .BaseImageDigest }}`
//! template surface, which is primarily intended for OCI annotations:
//!
//! ```yaml
//! dockers_v2:
//!   - annotations:
//!       org.opencontainers.image.base.name:   "{{ .BaseImage }}"
//!       org.opencontainers.image.base.digest: "{{ .BaseImageDigest }}"
//! ```
//!
//! The parser is intentionally *partial* — it implements only the subset of
//! Dockerfile syntax needed to identify the final stage's base image. The
//! real `docker buildx build` invocation is the source of truth; if the
//! parser fails to recognise an exotic construct, the annotation simply
//! won't be set.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::process::Command;

use anodizer_core::log::StageLogger;
use anodizer_core::run::run_capture_timeout;
use anyhow::{Context as _, Result};
use regex::Regex;
use std::sync::LazyLock;
use std::time::Duration;

/// Wall-clock bound on `docker buildx imagetools inspect` — a base-image digest
/// lookup against the remote registry. An unreachable or stalled registry would
/// otherwise hang the build forever with no exit; on expiry the probe subtree is
/// killed and the caller falls back to an empty digest (best-effort). Sized as a
/// remote metadata fetch.
const IMAGETOOLS_INSPECT_TIMEOUT: Duration = Duration::from_secs(300);

/// Result of resolving a Dockerfile's final base image.
#[derive(Debug, Clone, Default)]
pub struct BaseImage {
    /// The fully-substituted base image reference (e.g. `alpine:3.20`,
    /// `alpine@sha256:...`). Empty when no resolvable `FROM` directive is
    /// found (or the final stage is `scratch`).
    pub name: String,
    /// The image manifest digest (`sha256:...`). Empty when the base image
    /// can't be resolved or the digest probe fails.
    pub digest: String,
}

// Case-insensitive `ARG NAME[=DEFAULT]`. The default value may include `=`
// (e.g. `ARG TAG=3.20-x86_64`), so the trailing capture is greedy. Anchored
// to a single logical line — line-continuation joining happens before this
// regex runs but stops at the next directive boundary, so the greedy `.*`
// can't swallow a subsequent `FROM`.
static ARG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^ARG\s+([A-Za-z_][A-Za-z0-9_]*)(?:=(.*))?$").expect("static regex")
});

// Case-insensitive `FROM [--flag=value ...] <image> [AS <alias>]`.
// `--platform=$BUILDPLATFORM` and similar `--foo=bar` flags are stripped
// before the image reference.
static FROM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^FROM(?:\s+--\S+)*\s+(\S+)(?:\s+AS\s+(\S+))?\s*$").expect("static regex")
});

// 64 lowercase hex chars after the `sha256:` prefix.
static SHA256_HEX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9a-f]{64}$").expect("static regex"));

/// Parse the final stage's base image from raw Dockerfile content.
///
/// Returns an empty string when no usable `FROM` directive is found
/// (empty file, comments-only, `RUN`-only). The literal value `scratch`
/// is returned verbatim — callers must short-circuit it before any
/// digest-resolution call.
///
/// Base-image reference parsing.
/// `parseBaseImage`:
///
/// - Line-continuation (`\\` + newline) joins physical lines back into a
///   single logical directive, but joining stops at the next directive
///   boundary — so `ARG VER=3.20 \\` followed by `FROM ...` is two
///   directives, not one. This avoids the greedy `(.*)` in `ARG_RE`
///   swallowing the subsequent `FROM` line.
/// - Comments (`#`-prefixed lines) and blank lines are skipped.
/// - Global `ARG NAME[=DEFAULT]` directives (those appearing before any
///   `FROM`) populate a substitution table consulted by every subsequent
///   `FROM`.
/// - `FROM` keyword is case-insensitive, as are any `AS <alias>` clauses
///   and the alias-chain resolution that follows.
/// - `--platform=...` / `--foo=bar` flags between `FROM` and the image
///   reference are stripped.
/// - `${NAME}`, `${NAME:-default}` and `$NAME` ARG references inside the
///   image string are substituted with the ARG table (defaulting to the
///   inline `:-default`, then to empty). The bare-dash `${NAME-default}`
///   form is NOT supported (the `:-` separator is matched literally).
/// - Alias chains (`FROM a AS b` followed by `FROM b`) walk to the root;
///   cyclic aliases (`a→b, b→a`) terminate on revisit via a visited set.
pub fn parse_base_image(content: &str) -> String {
    let logical_lines = join_continuations(content);

    let mut args: HashMap<String, String> = HashMap::new();
    let mut aliases: HashMap<String, String> = HashMap::new();
    let mut base = String::new();

    for raw_line in &logical_lines {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if base.is_empty()
            && let Some(caps) = ARG_RE.captures(line)
        {
            let Some(name_m) = caps.get(1) else { continue };
            let name = name_m.as_str().to_string();
            let default = caps
                .get(2)
                .map(|m| trim_quotes(m.as_str()).to_string())
                .unwrap_or_default();
            args.insert(name, default);
            continue;
        }

        if let Some(caps) = FROM_RE.captures(line) {
            let Some(img_m) = caps.get(1) else { continue };
            base = substitute_args(img_m.as_str(), &args);
            if let Some(alias) = caps.get(2) {
                aliases.insert(alias.as_str().to_ascii_lowercase(), base.clone());
            }
        }
    }

    // Walk alias chain with a visited set so cyclic aliases terminate
    // deterministically. The chain length is bounded by the number of
    // distinct aliases; revisiting a name means we've closed a cycle.
    let mut visited: HashSet<String> = HashSet::new();
    loop {
        let key = base.to_ascii_lowercase();
        if !visited.insert(key.clone()) {
            break;
        }
        match aliases.get(&key) {
            Some(next) if next != &base => base = next.clone(),
            _ => break,
        }
    }
    base
}

/// Join physical lines that end in `\` into a single logical line, but
/// stop joining at a directive boundary. A line that ends in `\` followed
/// by a line whose first non-whitespace token is a Dockerfile directive
/// keyword (`ARG`, `FROM`, `RUN`, `COPY`, ...) is treated as a malformed
/// continuation and split: the trailing `\` is dropped and the next
/// directive starts a new logical line.
///
/// This guards against the failure mode where `ARG VER=3.20 \\` followed
/// by `FROM alpine:${VER}` collapses into one line, letting `ARG_RE`'s
/// greedy `(.*)` swallow the `FROM` as part of the ARG default.
fn join_continuations(content: &str) -> Vec<String> {
    let raw_lines: Vec<&str> = content.split('\n').collect();
    let mut out: Vec<String> = Vec::new();
    let mut buf = String::new();

    for line in raw_lines {
        let ends_with_continuation = line.trim_end().ends_with('\\');
        if ends_with_continuation {
            // Strip the trailing backslash plus any trailing whitespace
            // before it; the joining space replaces both.
            let trimmed = line.trim_end();
            let without_slash = &trimmed[..trimmed.len() - 1];
            buf.push_str(without_slash);
            buf.push(' ');
            continue;
        }
        buf.push_str(line);
        // If buf starts with a directive AND the joined result also
        // contains a later directive keyword, the second one is a stray
        // — flush the first and re-process the rest. This only happens
        // when an earlier line had a continuation but the next line was
        // itself a new directive. Detect by scanning for a second
        // directive-start in the buffer.
        let logical = std::mem::take(&mut buf);
        for piece in split_at_directive_boundary(&logical) {
            out.push(piece);
        }
    }
    if !buf.is_empty() {
        for piece in split_at_directive_boundary(&buf) {
            out.push(piece);
        }
    }
    out
}

/// Split a logically-joined line at any second directive keyword. Returns
/// at minimum the input as a single element when no internal directive is
/// found. The first token (if a directive keyword) anchors the line; any
/// later token matching a directive keyword starts a new logical line.
fn split_at_directive_boundary(line: &str) -> Vec<String> {
    const DIRECTIVES: &[&str] = &[
        "FROM",
        "ARG",
        "RUN",
        "CMD",
        "LABEL",
        "MAINTAINER",
        "EXPOSE",
        "ENV",
        "ADD",
        "COPY",
        "ENTRYPOINT",
        "VOLUME",
        "USER",
        "WORKDIR",
        "ONBUILD",
        "STOPSIGNAL",
        "HEALTHCHECK",
        "SHELL",
    ];
    let trimmed = line.trim_start();
    let leading_ws_len = line.len() - trimmed.len();

    // Find the first directive token; this is the anchor and is not a
    // split point. Then look for a subsequent directive token to split on.
    let mut tokens = trimmed.split_whitespace();
    let Some(first) = tokens.next() else {
        return vec![line.to_string()];
    };
    let first_upper = first.to_ascii_uppercase();
    if !DIRECTIVES.contains(&first_upper.as_str()) {
        return vec![line.to_string()];
    }

    // Walk subsequent whitespace-bounded tokens in `after_first`,
    // looking for any further directive keyword. The first hit is the
    // split boundary; everything before becomes the head, everything
    // from that token on becomes a fresh logical line.
    let after_first = &trimmed[first.len()..];
    let mut split_at: Option<usize> = None;
    let mut token_start: Option<usize> = None;
    for (idx, ch) in after_first.char_indices() {
        if ch.is_whitespace() {
            if let Some(start) = token_start.take() {
                let token = &after_first[start..idx];
                if DIRECTIVES.contains(&token.to_ascii_uppercase().as_str()) {
                    split_at = Some(leading_ws_len + first.len() + start);
                    break;
                }
            }
        } else if token_start.is_none() {
            token_start = Some(idx);
        }
    }
    // Trailing token (no whitespace after it).
    if split_at.is_none()
        && let Some(start) = token_start
    {
        let token = &after_first[start..];
        if DIRECTIVES.contains(&token.to_ascii_uppercase().as_str()) {
            split_at = Some(leading_ws_len + first.len() + start);
        }
    }

    match split_at {
        Some(pos) => {
            let head = line[..pos].trim_end().to_string();
            let tail = line[pos..].trim_start().to_string();
            let mut v = vec![head];
            v.extend(split_at_directive_boundary(&tail));
            v
        }
        None => vec![line.to_string()],
    }
}

/// Strip any leading / trailing `"` or `'` characters from `s` until
/// neither remains on either end.
/// `strings.Trim(m[2], "\"'")` semantic.
fn trim_quotes(s: &str) -> &str {
    s.trim_matches(|c: char| c == '"' || c == '\'')
}

/// Substitute `$NAME` and `${NAME[:-default]}` references in `s` using
/// the ARG table via shell-style `${VAR}` expansion and a `:-` cut on the
/// name. The bare-dash `${NAME-default}` form is intentionally NOT
/// recognised: expansion only sees the `${...}` body, and the cut splits
/// on the literal substring `":-"`, not `"-"`.
fn substitute_args(s: &str, args: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.char_indices().peekable();
    while let Some((i, ch)) = chars.next() {
        if ch != '$' {
            out.push(ch);
            continue;
        }
        // `$` at end of string: emit literal.
        let Some(&(_, next_ch)) = chars.peek() else {
            out.push('$');
            continue;
        };
        if next_ch == '{' {
            // `${...}` form. The bracket body is ASCII-`{`/`}` bounded;
            // searching for the closing `}` in the byte slice is safe
            // because `}` is a single-byte ASCII codepoint and char
            // boundaries align.
            let body_start_byte = i + '$'.len_utf8() + '{'.len_utf8();
            if let Some(close_rel) = s[body_start_byte..].find('}') {
                let inner = &s[body_start_byte..body_start_byte + close_rel];
                out.push_str(&resolve_expand(inner, args));
                // Advance the iterator past the closing `}`.
                let end_byte = body_start_byte + close_rel + '}'.len_utf8();
                while let Some(&(j, _)) = chars.peek() {
                    if j >= end_byte {
                        break;
                    }
                    chars.next();
                }
            } else {
                // Unclosed `${`: emit literal `$`.
                out.push('$');
            }
        } else if next_ch.is_ascii_alphabetic() || next_ch == '_' {
            // `$NAME` form. Consume an ASCII identifier.
            let ident_start_byte = i + '$'.len_utf8();
            let mut end_byte = ident_start_byte;
            let mut iter = s[ident_start_byte..].char_indices().peekable();
            while let Some(&(j, c)) = iter.peek() {
                if c.is_ascii_alphanumeric() || c == '_' {
                    end_byte = ident_start_byte + j + c.len_utf8();
                    iter.next();
                } else {
                    break;
                }
            }
            let name = &s[ident_start_byte..end_byte];
            out.push_str(&resolve_expand(name, args));
            // Advance the outer iterator past the identifier.
            while let Some(&(j, _)) = chars.peek() {
                if j >= end_byte {
                    break;
                }
                chars.next();
            }
        } else {
            // Not a valid identifier — emit `$` literally.
            out.push('$');
        }
    }
    out
}

fn resolve_expand(token: &str, args: &HashMap<String, String>) -> String {
    let (key, default) = match token.split_once(":-") {
        Some((k, d)) => (k, d),
        None => (token, ""),
    };
    match args.get(key) {
        Some(v) if !v.is_empty() => v.clone(),
        _ => default.to_string(),
    }
}

/// Read a Dockerfile and resolve its final-stage base image plus the
/// image's manifest digest. Returns `Ok(None)` when the Dockerfile has
/// no resolvable base image (empty, no `FROM`, or `FROM scratch`).
///
/// When the `FROM` already pins a digest (`image@sha256:...`), that
/// digest is returned without any external lookup. Otherwise — when not
/// in dry-run — `docker buildx imagetools inspect <ref>` is invoked to
/// resolve the manifest digest. A failed probe emits a warning via
/// `log` and returns the image name with an empty digest (best-effort:
/// callers can still set `org.opencontainers.image.base.name` even
/// without the digest).
pub fn get_base_image(
    dockerfile: &Path,
    dry_run: bool,
    log: &StageLogger,
) -> Result<Option<BaseImage>> {
    let content = fs::read_to_string(dockerfile)
        .with_context(|| format!("read dockerfile {}", dockerfile.display()))?;
    let base = parse_base_image(&content);
    if base.is_empty() || base.eq_ignore_ascii_case("scratch") {
        return Ok(None);
    }

    // Already-pinned `image@sha256:...` references carry their own digest;
    // no network probe needed.
    if let Some((_, after)) = base.split_once('@')
        && after.starts_with("sha256:")
    {
        return Ok(Some(BaseImage {
            name: base.clone(),
            digest: after.to_string(),
        }));
    }

    if dry_run {
        return Ok(Some(BaseImage {
            name: base,
            digest: String::new(),
        }));
    }

    match resolve_digest(&base, log) {
        Ok(digest) => Ok(Some(BaseImage { name: base, digest })),
        Err(e) => {
            log.warn(&format!(
                "could not resolve base image digest for {base}: {e:#}"
            ));
            Ok(Some(BaseImage {
                name: base,
                digest: String::new(),
            }))
        }
    }
}

/// Shell out to `docker buildx imagetools inspect <ref>
/// --format "{{.Manifest.Digest}}"` and capture the digest. Validates
/// that the output is a single line of the form `sha256:<64 lowercase
/// hex chars>`; any other shape is rejected with a typed error so a
/// buggy or compromised `docker` binary can't smuggle arbitrary bytes
/// into the `BaseImageDigest` template variable (and from there, into
/// OCI annotations).
fn resolve_digest(reference: &str, log: &StageLogger) -> Result<String> {
    let mut cmd = Command::new("docker");
    cmd.args([
        "buildx",
        "imagetools",
        "inspect",
        reference,
        "--format",
        "{{.Manifest.Digest}}",
    ]);
    // Bounded: the inspect hits the remote registry, so an unreachable registry
    // must not hang the build with no deadline. The caller treats any Err
    // (including a deadline kill) as a best-effort miss and warns.
    let output = run_capture_timeout(
        &mut cmd,
        log,
        "docker buildx imagetools inspect",
        IMAGETOOLS_INSPECT_TIMEOUT,
    )
    .with_context(|| format!("spawn docker buildx imagetools inspect {reference}"))?;

    if !output.status.success() {
        anyhow::bail!(
            "docker buildx imagetools inspect {reference} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim(),
        );
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    // Reject multi-line outputs: a well-behaved buildx prints exactly
    // one line. Any second non-empty line is a signal that the output
    // shape changed (or stderr leaked into stdout).
    let mut lines = raw.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines.next().unwrap_or("");
    if lines.next().is_some() {
        anyhow::bail!("multi-line digest output for {reference}: {raw:?}");
    }
    validate_digest(reference, first)?;
    Ok(first.to_string())
}

fn validate_digest(reference: &str, digest: &str) -> Result<()> {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        anyhow::bail!("unexpected digest output for {reference}: {digest:?}");
    };
    if !SHA256_HEX_RE.is_match(hex) {
        anyhow::bail!("invalid sha256 hex in digest for {reference}: {digest:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::log::Verbosity;

    fn test_logger() -> StageLogger {
        StageLogger::new("baseimage-test", Verbosity::from_flags(true, false, false))
    }

    // Base-image parse fixtures.
    // `internal/pipe/docker/v2/testdata/dockerfiles/`. Each pair (name,
    // content) is asserted against the expected `parseBaseImage` output
    // in `expected_for_fixture` below — the same assertion table used by
    // Base-image parse cases.
    const FIXTURES: &[(&str, &str)] = &[
        ("empty", ""),
        ("comment", "# FROM alpine\n"),
        ("no-from", "RUN echo hi\n"),
        ("simple", "FROM alpine:3.20\n"),
        ("with-digest", "FROM alpine@sha256:abc123\n"),
        (
            "with-platform-flag",
            "FROM --platform=linux/amd64 alpine:3.20\n",
        ),
        (
            "multiple-flags",
            "FROM --platform=$BUILDPLATFORM --foo=bar alpine:3.20 AS builder\n",
        ),
        (
            "multi-stage",
            "FROM golang:1.22 AS builder\nFROM alpine:3.20\n",
        ),
        (
            "follows-alias",
            "FROM alpine:3.20 AS base\nFROM base AS final\n",
        ),
        (
            "alias-chain",
            "FROM alpine:3.20 AS a\nFROM a AS b\nFROM b AS c\nFROM c\n",
        ),
        (
            "alias-case-insensitive",
            "FROM alpine:3.20 AS Base\nfrom BASE\n",
        ),
        ("arg-simple", "ARG VER=3.20\nFROM alpine:${VER}\n"),
        ("arg-with-default", "ARG VER\nFROM alpine:${VER:-3.20}\n"),
        ("arg-dollar-form", "ARG IMG=alpine:3.20\nFROM $IMG\n"),
        (
            "arg-after-from",
            "FROM alpine:3.20\nARG VER=3.21\nFROM alpine:${VER:-3.19}\n",
        ),
        (
            "line-continuation",
            "FROM \\\n  alpine:3.20 \\\n  AS base\n",
        ),
        ("scratch", "FROM scratch\n"),
        ("lowercase-from", "from alpine:3.20\n"),
        (
            "quoted-arg-default",
            "ARG IMG=\"alpine:3.20\"\nFROM ${IMG}\n",
        ),
    ];

    fn expected_for_fixture(name: &str) -> &'static str {
        match name {
            "empty" | "comment" | "no-from" => "",
            "with-digest" => "alpine@sha256:abc123",
            "scratch" => "scratch",
            "arg-after-from" => "alpine:3.19",
            _ => "alpine:3.20",
        }
    }

    #[test]
    fn parse_base_image_matches_goreleaser_fixtures() {
        for (name, content) in FIXTURES {
            let got = parse_base_image(content);
            assert_eq!(got, expected_for_fixture(name), "fixture {name} mismatch");
        }
    }

    #[test]
    fn get_base_image_returns_none_for_scratch() {
        let dir = tempfile::tempdir().unwrap();
        let dockerfile = dir.path().join("Dockerfile");
        std::fs::write(&dockerfile, "FROM scratch\n").unwrap();
        let got = get_base_image(&dockerfile, true, &test_logger()).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn get_base_image_returns_none_for_empty() {
        let dir = tempfile::tempdir().unwrap();
        let dockerfile = dir.path().join("Dockerfile");
        std::fs::write(&dockerfile, "").unwrap();
        let got = get_base_image(&dockerfile, true, &test_logger()).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn get_base_image_pinned_digest_skips_network() {
        let dir = tempfile::tempdir().unwrap();
        let dockerfile = dir.path().join("Dockerfile");
        let pinned =
            "alpine@sha256:4bcff63911fcb4448bd4fdacec207030997caf25e9bea4045fa6c8c44de311d1";
        std::fs::write(&dockerfile, format!("FROM {pinned}\n")).unwrap();
        // `dry_run=false` here is safe because the pinned-digest branch
        // returns before any subprocess spawn — same contract as the
        // case: digest pinned in FROM.
        let got = get_base_image(&dockerfile, false, &test_logger())
            .unwrap()
            .unwrap();
        assert_eq!(got.name, pinned);
        assert_eq!(
            got.digest,
            "sha256:4bcff63911fcb4448bd4fdacec207030997caf25e9bea4045fa6c8c44de311d1"
        );
    }

    #[test]
    fn get_base_image_dry_run_skips_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let dockerfile = dir.path().join("Dockerfile");
        std::fs::write(&dockerfile, "FROM alpine:3.20\n").unwrap();
        let got = get_base_image(&dockerfile, true, &test_logger())
            .unwrap()
            .unwrap();
        assert_eq!(got.name, "alpine:3.20");
        assert_eq!(got.digest, "");
    }

    // Regression test: the `:-` separator only matches
    // the `:-` form. The bare-dash `${VER-3.20}` form must resolve to
    // empty when `VER` is unset, matching `os.Expand` semantics.
    #[test]
    fn parse_base_image_arg_bare_dash_default_resolves_to_empty() {
        let got = parse_base_image("ARG VER\nFROM alpine:${VER-3.20}\n");
        assert_eq!(got, "alpine:");
    }

    #[test]
    fn parse_base_image_arg_overrides_default() {
        let got = parse_base_image("ARG VER=3.20\nFROM alpine:${VER:-3.19}\n");
        assert_eq!(got, "alpine:3.20");
    }

    #[test]
    fn parse_base_image_unclosed_brace_is_literal() {
        // `${UNCLOSED` with no `}` is emitted as-is; this guards against
        // an infinite-loop or panic regression in `substitute_args`.
        let got = parse_base_image("FROM alpine:${UNCLOSED\n");
        assert_eq!(got, "alpine:${UNCLOSED");
    }

    // B1 regression test: non-ASCII bytes must round-trip through the
    // expander without Latin-1 corruption. `imáge` survives.
    #[test]
    fn parse_base_image_preserves_utf8() {
        let got = parse_base_image("FROM imáge:3.20\n");
        assert_eq!(got, "imáge:3.20");
    }

    #[test]
    fn parse_base_image_preserves_utf8_in_arg_default() {
        let got = parse_base_image("ARG IMG=imáge:3.20\nFROM ${IMG}\n");
        assert_eq!(got, "imáge:3.20");
    }

    // B3 regression test: an ARG line with a trailing `\` followed by a
    // FROM line must NOT collapse into one logical directive — otherwise
    // the greedy `(.*)` in `ARG_RE` would swallow the FROM.
    #[test]
    fn parse_base_image_continuation_does_not_swallow_from() {
        let got = parse_base_image("ARG VER=3.20 \\\nFROM alpine:${VER}\n");
        assert_eq!(got, "alpine:3.20");
    }

    // B4 regression test: a cyclic alias chain (`a→b, b→a`) must
    // terminate deterministically rather than dangle on iteration
    // bound. The resolved base settles on whichever name is reached
    // first after the cycle is detected.
    #[test]
    fn parse_base_image_alias_cycle_terminates() {
        let got = parse_base_image("FROM alpine:3.20 AS a\nFROM a AS b\nFROM b AS a\n");
        // Final FROM is `b AS a`. Walking aliases: a→b, b→a. From base
        // `a` we visit a→b, then b→a (revisit `a`) → stop. The walk
        // halts without panic or hang.
        assert!(got == "a" || got == "b" || got == "alpine:3.20");
    }

    // B6 regression test: mixed/multiple outer quotes are stripped to
    // bare value, trimming surrounding quotes.
    #[test]
    fn trim_quotes_strips_mixed_layers() {
        assert_eq!(trim_quotes("\"'alpine:3.20'\""), "alpine:3.20");
        assert_eq!(trim_quotes("'\"alpine:3.20\"'"), "alpine:3.20");
        assert_eq!(trim_quotes("\"\"alpine:3.20\"\""), "alpine:3.20");
        assert_eq!(trim_quotes("alpine:3.20"), "alpine:3.20");
    }

    // B7 regression test: digest validator rejects non-hex, wrong-length,
    // and uppercase-hex outputs.
    #[test]
    fn validate_digest_accepts_valid_sha256() {
        let ok = "sha256:4bcff63911fcb4448bd4fdacec207030997caf25e9bea4045fa6c8c44de311d1";
        assert!(validate_digest("ref", ok).is_ok());
    }

    #[test]
    fn validate_digest_rejects_short_hex() {
        assert!(validate_digest("ref", "sha256:abc123").is_err());
    }

    #[test]
    fn validate_digest_rejects_uppercase_hex() {
        let bad = "sha256:4BCFF63911FCB4448BD4FDACEC207030997CAF25E9BEA4045FA6C8C44DE311D1";
        assert!(validate_digest("ref", bad).is_err());
    }

    #[test]
    fn validate_digest_rejects_non_hex() {
        let bad = "sha256:zzzzzz3911fcb4448bd4fdacec207030997caf25e9bea4045fa6c8c44de311d1";
        assert!(validate_digest("ref", bad).is_err());
    }

    #[test]
    fn validate_digest_rejects_missing_prefix() {
        let bad = "4bcff63911fcb4448bd4fdacec207030997caf25e9bea4045fa6c8c44de311d1";
        assert!(validate_digest("ref", bad).is_err());
    }
}
