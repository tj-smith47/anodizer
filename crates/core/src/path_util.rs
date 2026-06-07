//! Path-string utilities shared across config loading, env-file reading, and
//! the template engine.

use std::borrow::Cow;
use std::path::PathBuf;

/// Resolve the current user's home directory from the environment.
///
/// Prefers `$HOME` (set on every POSIX shell and on Windows under most CI /
/// MSYS setups), falling back to `%USERPROFILE%` on Windows where `$HOME`
/// is frequently unset. Empty values are treated as unset so a stray
/// `HOME=` export does not collapse `~/foo` into `/foo`.
fn home_dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        return Some(PathBuf::from(home));
    }
    std::env::var_os("USERPROFILE")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// Expand a leading `~` into the user's home directory.
///
/// `~` is rewritten only when it appears at the very start of `path` AND is
/// followed by `/` (or end-of-string), mirroring the POSIX-shell
/// word-initial tilde rule; anywhere else the literal `~` is preserved so a
/// path like `./safe~backup.yaml` survives untouched.
///
/// `~user/...` (POSIX user-home form) is NOT supported — resolving an
/// arbitrary user's home requires a `getpwnam(3)` call (or platform
/// equivalent) that anodizer deliberately avoids for the security and
/// cross-platform-portability cost; such a path is returned unchanged.
///
/// The home directory is sourced from `$HOME`, falling back to
/// `%USERPROFILE%` on Windows. When neither is set (or `path` has no leading
/// `~/`), the input is returned unchanged. A `Cow::Borrowed` is returned for
/// the non-expanding case to avoid an allocation.
pub fn expand_tilde(path: &str) -> Cow<'_, str> {
    if let Some(rest) = path.strip_prefix('~')
        && (rest.is_empty() || rest.starts_with('/'))
        && let Some(home) = home_dir()
    {
        let rest_trimmed = rest.strip_prefix('/').unwrap_or(rest);
        // `home.join("")` would append a trailing separator, so bare `~` and
        // `~/` must short-circuit to the home directory itself.
        let resolved = if rest_trimmed.is_empty() {
            home
        } else {
            home.join(rest_trimmed)
        };
        return Cow::Owned(resolved.to_string_lossy().into_owned());
    }
    Cow::Borrowed(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mutating process env is racy across threads; the unsafe blocks below are
    // required by the Rust 2024 edition's `set_var`/`remove_var` signatures.
    // These tests run single-threaded relative to each other only by virtue of
    // each restoring the env it touched before returning.

    #[test]
    #[serial_test::serial]
    fn expands_leading_tilde_slash() {
        unsafe {
            std::env::set_var("HOME", "/home/tester");
        }
        let expected = PathBuf::from("/home/tester")
            .join("x")
            .to_string_lossy()
            .into_owned();
        assert_eq!(expand_tilde("~/x"), expected);
    }

    #[test]
    #[serial_test::serial]
    fn expands_bare_tilde() {
        unsafe {
            std::env::set_var("HOME", "/home/tester");
        }
        assert_eq!(expand_tilde("~"), "/home/tester");
    }

    #[test]
    fn passes_through_non_tilde_path() {
        assert_eq!(expand_tilde("/etc/anodize.yaml"), "/etc/anodize.yaml");
        assert_eq!(expand_tilde("./safe~backup.yaml"), "./safe~backup.yaml");
    }

    #[test]
    #[serial_test::serial]
    fn user_home_form_not_expanded() {
        unsafe {
            std::env::set_var("HOME", "/home/tester");
        }
        assert_eq!(expand_tilde("~bob/foo"), "~bob/foo");
        assert_eq!(expand_tilde("~bob"), "~bob");
    }

    #[test]
    #[serial_test::serial]
    fn falls_back_to_userprofile() {
        let saved_home = std::env::var_os("HOME");
        let saved_profile = std::env::var_os("USERPROFILE");
        unsafe {
            std::env::remove_var("HOME");
            std::env::set_var("USERPROFILE", "/Users/winuser");
        }
        let got = expand_tilde("~/docs").into_owned();
        unsafe {
            match saved_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match saved_profile {
                Some(v) => std::env::set_var("USERPROFILE", v),
                None => std::env::remove_var("USERPROFILE"),
            }
        }
        let expected = PathBuf::from("/Users/winuser")
            .join("docs")
            .to_string_lossy()
            .into_owned();
        assert_eq!(got, expected);
    }
}
