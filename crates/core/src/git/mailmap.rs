//! `.mailmap`-driven author-identity canonicalization.
//!
//! A repo's `.mailmap` declares that several `Name <email>` identities are
//! the same person (typically because a placeholder or unlinked email was
//! used for some commits). The changelog login enricher uses this to lend a
//! resolved GitHub login from one alias to its login-less siblings, so every
//! commit of a single author renders the same `@login` mention.
//!
//! This wraps `git check-mailmap`, which reads the repo's `.mailmap` and maps
//! a raw `Name <email>` to its canonical `Name <email>`. It is strictly
//! best-effort: a missing git binary, a non-repo directory, or no `.mailmap`
//! leaves the identity unchanged (the helper returns `None`), so callers treat
//! the absence of a mailmap as a no-op.

use std::path::Path;
use std::process::Command;

/// Canonicalize an author identity to its `.mailmap` email in `cwd`'s repo.
///
/// Returns the canonical EMAIL for `name <email>` per `cwd`'s `.mailmap`, or
/// `None` when git is unavailable, `cwd` is not a repo, the output can't be
/// parsed, or the canonical email is empty. When no `.mailmap` entry matches,
/// `git check-mailmap` echoes the input unchanged, so the returned email
/// equals the input `email` — making an absent mailmap a no-op for callers
/// that compare canonical emails.
pub fn canonical_author_email_in(cwd: &Path, name: &str, email: &str) -> Option<String> {
    if email.is_empty() {
        return None;
    }
    // `git check-mailmap --stdin` reads `Name <email>` lines and writes the
    // canonical `Name <email>` per the repo's `.mailmap`. Feeding via stdin
    // avoids any argv quoting of the contact line. `LC_ALL=C` keeps any
    // diagnostic locale-stable; `GIT_TERMINAL_PROMPT=0` prevents a credential
    // helper from blocking an unattended run.
    let mut child = Command::new("git")
        .current_dir(cwd)
        .args(["check-mailmap", "--stdin"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    {
        use std::io::Write as _;
        let mut stdin = child.stdin.take()?;
        writeln!(stdin, "{name} <{email}>").ok()?;
    }

    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&output.stdout);
    parse_contact_email(line.trim())
}

/// Extract the email from a `Name <email>` contact line, returning `None` for
/// a missing or empty `<...>` segment.
fn parse_contact_email(line: &str) -> Option<String> {
    let open = line.rfind('<')?;
    let close = line[open..].find('>')? + open;
    let email = line[open + 1..close].trim();
    if email.is_empty() {
        None
    } else {
        Some(email.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.current_dir(dir)
                    .args(args)
                    .env("GIT_TERMINAL_PROMPT", "0");
                cmd
            },
            "git",
        );
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn init_repo_with_mailmap(dir: &Path, mailmap: &str) {
        git(dir, &["init", "-q"]);
        git(dir, &["config", "user.email", "test@test.com"]);
        git(dir, &["config", "user.name", "test"]);
        std::fs::write(dir.join(".mailmap"), mailmap).unwrap();
    }

    #[test]
    fn maps_aliased_email_to_canonical() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo_with_mailmap(tmp.path(), "TJ Smith <tj@jarvispro.io> <jane@work.com>\n");
        assert_eq!(
            canonical_author_email_in(tmp.path(), "TJ Smith", "jane@work.com"),
            Some("tj@jarvispro.io".to_string()),
            "aliased email canonicalizes to the primary identity"
        );
    }

    #[test]
    fn unmapped_email_passes_through_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo_with_mailmap(tmp.path(), "TJ Smith <tj@jarvispro.io> <jane@work.com>\n");
        assert_eq!(
            canonical_author_email_in(tmp.path(), "Someone", "nobody@example.com"),
            Some("nobody@example.com".to_string()),
            "an unmapped identity echoes back unchanged (no-op)"
        );
    }

    #[test]
    fn no_mailmap_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-q"]);
        git(tmp.path(), &["config", "user.email", "test@test.com"]);
        git(tmp.path(), &["config", "user.name", "test"]);
        assert_eq!(
            canonical_author_email_in(tmp.path(), "TJ Smith", "jane@work.com"),
            Some("jane@work.com".to_string()),
            "without a .mailmap the input email is returned verbatim"
        );
    }

    #[test]
    fn non_repo_dir_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            canonical_author_email_in(tmp.path(), "TJ Smith", "jane@work.com"),
            None,
            "a non-repo directory disables canonicalization (best-effort)"
        );
    }

    #[test]
    fn empty_email_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo_with_mailmap(tmp.path(), "TJ Smith <tj@x.io> <j@w.com>\n");
        assert_eq!(canonical_author_email_in(tmp.path(), "TJ Smith", ""), None);
    }

    #[test]
    fn parse_contact_email_extracts_address() {
        assert_eq!(
            parse_contact_email("TJ Smith <tj@x.io>"),
            Some("tj@x.io".to_string())
        );
        assert_eq!(parse_contact_email("No Email Here"), None);
        assert_eq!(parse_contact_email("Empty <>"), None);
    }
}
