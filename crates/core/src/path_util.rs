//! Path-string utilities shared across config loading, env-file reading, and
//! the template engine.

use crate::EnvSource;
use std::borrow::Cow;
use std::path::PathBuf;

/// Resolve the current user's home directory from the environment.
///
/// Prefers `$HOME` (set on every POSIX shell and on Windows under most CI /
/// MSYS setups), falling back to `%USERPROFILE%` on Windows where `$HOME`
/// is frequently unset. Empty values are treated as unset so a stray
/// `HOME=` export does not collapse `~/foo` into `/foo`.
fn home_dir_with_env<E: EnvSource + ?Sized>(env: &E) -> Option<PathBuf> {
    if let Some(home) = env.var("HOME").filter(|h| !h.is_empty()) {
        return Some(PathBuf::from(home));
    }
    env.var("USERPROFILE")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// A guaranteed-to-exist working directory for cwd-agnostic subprocess probes.
///
/// Detection probes like `rustc -vV`, `<tool> --version`, and
/// `docker buildx version` read nothing relative to the working directory, but
/// the spawned process still calls `getcwd()` at startup and aborts ("Could
/// not locate working directory") if the *inherited* cwd has been removed.
/// Tests that swap the process-global cwd into a tempdir and tear it down can
/// leave exactly that state, and a rotated/cleaned scratch dir can do so in
/// production. Pinning such probes to this directory makes them independent of
/// the inherited cwd. Returns the system temp dir, which always exists.
pub fn probe_dir() -> PathBuf {
    std::env::temp_dir()
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
    expand_tilde_with_env(path, &crate::ProcessEnvSource)
}

/// [`EnvSource`]-injecting form of [`expand_tilde`].
///
/// Resolves the home directory from `env` (`HOME`, then `USERPROFILE`)
/// instead of the process environment, so callers and tests can drive
/// tilde expansion deterministically without mutating global env state.
pub fn expand_tilde_with_env<'p, E: EnvSource + ?Sized>(path: &'p str, env: &E) -> Cow<'p, str> {
    if let Some(rest) = path.strip_prefix('~')
        && (rest.is_empty() || rest.starts_with('/'))
        && let Some(home) = home_dir_with_env(env)
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
    use crate::MapEnvSource;

    // Home-directory resolution is driven through an injected `MapEnvSource`
    // so these tests never touch the process environment and run race-free in
    // parallel with the rest of the crate's suite.

    #[test]
    fn expands_leading_tilde_slash() {
        let env = MapEnvSource::new().with("HOME", "/home/tester");
        let expected = PathBuf::from("/home/tester")
            .join("x")
            .to_string_lossy()
            .into_owned();
        assert_eq!(expand_tilde_with_env("~/x", &env), expected);
    }

    #[test]
    fn expands_bare_tilde() {
        let env = MapEnvSource::new().with("HOME", "/home/tester");
        assert_eq!(expand_tilde_with_env("~", &env), "/home/tester");
    }

    #[test]
    fn passes_through_non_tilde_path() {
        let env = MapEnvSource::new().with("HOME", "/home/tester");
        assert_eq!(
            expand_tilde_with_env("/etc/anodize.yaml", &env),
            "/etc/anodize.yaml"
        );
        assert_eq!(
            expand_tilde_with_env("./safe~backup.yaml", &env),
            "./safe~backup.yaml"
        );
    }

    #[test]
    fn user_home_form_not_expanded() {
        let env = MapEnvSource::new().with("HOME", "/home/tester");
        assert_eq!(expand_tilde_with_env("~bob/foo", &env), "~bob/foo");
        assert_eq!(expand_tilde_with_env("~bob", &env), "~bob");
    }

    #[test]
    fn falls_back_to_userprofile() {
        // HOME unset (absent from the map), USERPROFILE present.
        let env = MapEnvSource::new().with("USERPROFILE", "/Users/winuser");
        let got = expand_tilde_with_env("~/docs", &env).into_owned();
        let expected = PathBuf::from("/Users/winuser")
            .join("docs")
            .to_string_lossy()
            .into_owned();
        assert_eq!(got, expected);
    }
}
