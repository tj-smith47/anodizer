//! Path-string utilities shared across config loading, env-file reading, and
//! the template engine.

use crate::EnvSource;
use std::borrow::Cow;
use std::path::{Path, PathBuf};

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

/// The spelling of `path` relative to the repo root it lives under: what
/// anodizer prints for a repo-committed file, and what it hands `git add`.
///
/// Every path anodizer prints for a repo-committed file is the spelling the
/// user wrote (or would write) in their config, never the absolute path the
/// process happened to resolve — an absolute path leaks a runner's scratch
/// directory into logs a human reads and diffs. `root` is stripped when `path`
/// is under it; a path outside the root has no relative spelling, so it is
/// printed as-is. The repo root itself renders as `.`.
///
/// The bump commit stages the manifests it rewrote by this same spelling, so
/// the returned string is a git pathspec as well as a display string: any
/// formatting added for a reader's benefit would break staging.
pub fn display_under_root(root: &Path, path: &Path) -> String {
    // A path that was resolved through the filesystem (`canonicalize`) names
    // the real directory while `root` keeps the spelling the user gave, so a
    // symlinked root (macOS puts `$TMPDIR` under `/var`, a symlink to
    // `/private/var`) strips under neither spelling alone.
    let relative = match path.strip_prefix(root) {
        Ok(relative) => relative.to_path_buf(),
        Err(_) => std::fs::canonicalize(root)
            .ok()
            .and_then(|real_root| path.strip_prefix(real_root).ok().map(Path::to_path_buf))
            .unwrap_or_else(|| path.to_path_buf()),
    };
    // A `.` component survives `join` (`<root>/./Cargo.toml`), and a config
    // declaring the root crate as `.` is the common case — drop it so the
    // manifest prints as `Cargo.toml`, the spelling a reader would search for.
    let cleaned: PathBuf = relative
        .components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .collect();
    let rendered = cleaned.display().to_string();
    if rendered.is_empty() {
        ".".to_string()
    } else {
        rendered
    }
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

    /// A path resolved through a symlinked root still prints relative to the
    /// root as the user spelled it: `sync_workspace_deps` walks the canonical
    /// workspace, and on macOS every tempdir is reached through `/var` →
    /// `/private/var`.
    #[cfg(unix)]
    #[test]
    fn display_under_root_strips_a_symlinked_root() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir_all(real.join("crates/app")).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let resolved = std::fs::canonicalize(&link)
            .unwrap()
            .join("crates/app/Cargo.toml");
        assert_eq!(
            display_under_root(&link, &resolved),
            PathBuf::from("crates/app/Cargo.toml").display().to_string()
        );
    }

    /// The three spellings a message can face: a nested path under the root, the
    /// root's own manifest reached through a `.` crate dir, and a path that is
    /// not under the root at all (nothing to strip, so it prints in full).
    #[test]
    fn display_under_root_strips_the_root_and_the_dot() {
        let root = Path::new("/repo");
        assert_eq!(
            display_under_root(root, &root.join("crates/app").join("Cargo.toml")),
            PathBuf::from("crates/app/Cargo.toml").display().to_string()
        );
        assert_eq!(
            display_under_root(root, &root.join(".").join("Cargo.toml")),
            "Cargo.toml"
        );
        assert_eq!(display_under_root(root, &root.join(".")), ".");
        assert_eq!(
            display_under_root(root, Path::new("/elsewhere/Cargo.toml")),
            PathBuf::from("/elsewhere/Cargo.toml").display().to_string()
        );
    }

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
            expand_tilde_with_env("/etc/anodizer.yaml", &env),
            "/etc/anodizer.yaml"
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
