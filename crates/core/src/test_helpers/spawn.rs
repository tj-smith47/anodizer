//! Process-spawn helpers for tests: transient-spawn retry and the shared
//! `git` invocation wrappers built on it.

use std::path::Path;
use std::process::Command;

// NTSTATUS process-creation failure codes (as the i32 `ExitStatus::code()`
// surfaces them on Windows). Windows GitHub runners intermittently fail to
// *create* a child process under heavy parallel nextest load — desktop-heap /
// handle pressure makes the loader abort before the program's first
// instruction. The OS reports this as one of these NTSTATUS values, never as a
// program-level error: a real `git`/`node` failure returns 1 or 128. Retrying
// only on these codes therefore masks no genuine failure — the program never
// ran. Treated as i32 to match the sign-extended value `code()` returns.
#[cfg(windows)]
const TRANSIENT_SPAWN_CODES: &[i32] = &[
    0xC000_0142u32 as i32, // STATUS_DLL_INIT_FAILED
    0xC000_0135u32 as i32, // STATUS_DLL_NOT_FOUND
    0xC000_007Bu32 as i32, // STATUS_INVALID_IMAGE_FORMAT
    0xC000_0017u32 as i32, // STATUS_NO_MEMORY
    0xC000_0018u32 as i32, // STATUS_CONFLICTING_ADDRESSES
];

/// True iff `status` is a transient Windows process-creation failure (the OS
/// failing to *start* the child, not the child erroring).
///
/// Always `false` on non-Windows targets — these NTSTATUS codes are
/// Windows-only, and no Unix exit status is a "spawn-init failure".
#[cfg(windows)]
pub fn is_transient_spawn_failure(status: &std::process::ExitStatus) -> bool {
    status
        .code()
        .is_some_and(|c| TRANSIENT_SPAWN_CODES.contains(&c))
}

/// True iff `status` is a transient Windows process-creation failure (the OS
/// failing to *start* the child, not the child erroring).
///
/// Always `false` on non-Windows targets — these NTSTATUS codes are
/// Windows-only, and no Unix exit status is a "spawn-init failure".
#[cfg(not(windows))]
pub fn is_transient_spawn_failure(_status: &std::process::ExitStatus) -> bool {
    false
}

/// The global config every fixture `git` reads instead of the developer's.
///
/// Repository config, `-c` arguments and `GIT_AUTHOR_*` / `GIT_COMMITTER_*`
/// env all rank above it, so a fixture that chooses its own identity or
/// signing keeps it. `core.hooksPath` names the repository's own hooks
/// directory: a host-wide hooks directory is out of reach, and a test that
/// installs a hook under `.git/hooks` still sees it run.
pub const FIXTURE_GLOBAL_CONFIG: &str = "[user]
\tname = Anodizer Test
\temail = test@anodizer.local
[commit]
\tgpgsign = false
[tag]
\tgpgsign = false
[core]
\thooksPath = .git/hooks
[init]
\tdefaultBranch = master
";

/// The config `git init` and `git clone` copy into every repository a
/// fixture creates, through `GIT_TEMPLATE_DIR`.
///
/// A git the code under test spawns inside that repository reads the host's
/// global config, which this repository-level copy outranks, so a signing
/// host, a `core.hooksPath` and a missing identity are out of reach of the
/// production spawn too. `core.autocrlf` is left to the host on purpose: a
/// production clone writes the host's line endings, and a fixture whose own
/// config disagreed with it would read that clone as modified.
pub const FIXTURE_REPO_CONFIG: &str = "[user]
\tname = Anodizer Test
\temail = test@anodizer.local
[commit]
\tgpgsign = false
[tag]
\tgpgsign = false
[core]
\thooksPath = .git/hooks
";

/// The directory holding [`FIXTURE_GLOBAL_CONFIG`] as `gitconfig` and the
/// `template/` directory `git init` copies from.
///
/// One fixed directory per content version under the system temp dir, written
/// once and shared by every test process: a per-process temp dir would have
/// to be leaked for a child spawned at the end of a test to still find it.
pub fn fixture_git_dir() -> &'static Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let content_version = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            FIXTURE_GLOBAL_CONFIG.hash(&mut h);
            FIXTURE_REPO_CONFIG.hash(&mut h);
            h.finish()
        };
        let dir = std::env::temp_dir().join(format!("anodizer-fixture-git-{content_version:016x}"));
        let template = dir.join("template");
        std::fs::create_dir_all(template.join("hooks")).expect("fixture git dir");
        std::fs::create_dir_all(template.join("info")).expect("fixture git dir");
        for (path, body) in [
            (dir.join("gitconfig"), FIXTURE_GLOBAL_CONFIG),
            (template.join("config"), FIXTURE_REPO_CONFIG),
        ] {
            if std::fs::read_to_string(&path).ok().as_deref() != Some(body) {
                // Written beside and renamed over, so a parallel test process
                // never reads a half-written file.
                let staging = path.with_extension(format!("tmp{}", std::process::id()));
                std::fs::write(&staging, body).expect("fixture git config");
                std::fs::rename(&staging, &path).expect("fixture git config");
            }
        }
        dir
    })
}

/// Whether `program` names git: a bare `git`, `git.exe`, or a path to either.
fn is_git(program: &std::ffi::OsStr) -> bool {
    // Both separators are split on, so a Windows path reads the same on every
    // host that builds the test helpers.
    let text = program.to_string_lossy();
    let name = text.rsplit(['/', '\\']).next().unwrap_or("");
    let stem = name
        .strip_suffix(".exe")
        .or_else(|| name.strip_suffix(".EXE"))
        .unwrap_or(name);
    stem.eq_ignore_ascii_case("git")
}

/// Cut a `git` command off from the host's configuration, unless its builder
/// already chose its own.
///
/// A fixture `git commit` otherwise inherits the developer's
/// `commit.gpgsign`, `tag.gpgsign`, `gpg.program`, `user.signingkey` and
/// `core.hooksPath`, so the fixture's result depends on the host, and on a
/// signing host it runs `gpg` from a `PATH` another test may have stubbed.
///
/// - `GIT_CONFIG_GLOBAL` points at [`FIXTURE_GLOBAL_CONFIG`]: the file is
///   replaced whole, so a key nobody listed is covered too, and the fixed
///   identity in it means no spawn depends on the host's user database.
/// - `GIT_TEMPLATE_DIR` makes every `git init` and `git clone` copy
///   [`FIXTURE_REPO_CONFIG`] into the new repository, which is what reaches a
///   git the code under test spawns there.
/// - The system config is dropped on Unix, where it can hold anything. It is
///   kept on Windows, where it holds the platform defaults (`core.autocrlf`,
///   `core.symlinks`) that a production spawn in the same repository reads:
///   a fixture dropping them read a production clone as modified.
/// - Config handed down through the environment (`GIT_CONFIG_COUNT` /
///   `GIT_CONFIG_KEY_n` / `GIT_CONFIG_VALUE_n`, `GIT_CONFIG_PARAMETERS`) is
///   removed, so a test run started from a git hook or alias reads none of
///   its caller's settings.
///
/// The repository's own config and every `-c` argument still apply.
pub fn isolate_git_config(cmd: &mut Command) {
    if !is_git(cmd.get_program()) {
        return;
    }
    let chosen = |name: &str| cmd.get_envs().any(|(key, _)| key == name);
    let global = chosen("GIT_CONFIG_GLOBAL");
    let template = chosen("GIT_TEMPLATE_DIR");
    let system = chosen("GIT_CONFIG_NOSYSTEM") || chosen("GIT_CONFIG_SYSTEM");
    let parameters = chosen("GIT_CONFIG_PARAMETERS");
    let count = chosen("GIT_CONFIG_COUNT");
    let dir = fixture_git_dir();
    if !global {
        cmd.env("GIT_CONFIG_GLOBAL", dir.join("gitconfig"));
    }
    if !template {
        cmd.env("GIT_TEMPLATE_DIR", dir.join("template"));
    }
    if !system && !cfg!(windows) {
        cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    }
    if !parameters {
        cmd.env_remove("GIT_CONFIG_PARAMETERS");
    }
    if !count {
        cmd.env_remove("GIT_CONFIG_COUNT");
        let inherited: Vec<std::ffi::OsString> = std::env::vars_os()
            .map(|(key, _)| key)
            .filter(|key| {
                let key = key.to_string_lossy();
                key.starts_with("GIT_CONFIG_KEY_") || key.starts_with("GIT_CONFIG_VALUE_")
            })
            .collect();
        for key in inherited {
            cmd.env_remove(key);
        }
    }
}

/// Run a freshly-built [`Command`] to completion, retrying on transient Windows
/// process-creation failures.
///
/// `build` MUST construct a brand-new [`Command`] on each call — [`Command`] is
/// consumed by [`Command::output`], so a single instance cannot be reused
/// across attempts. `what` names the spawned tool for panic messages.
///
/// A `git` command is first passed through [`isolate_git_config`], so every
/// fixture spawn reads the same configuration on every host.
///
/// Retries up to 5 attempts only when [`is_transient_spawn_failure`] holds (a
/// Windows loader abort under parallel nextest load — see
/// [`TRANSIENT_SPAWN_CODES`]); a real program error is returned immediately and
/// unretried, so this masks no genuine failure. Backoff between attempts is
/// 50ms, 100ms, 200ms, 400ms.
pub fn output_with_spawn_retry(
    mut build: impl FnMut() -> Command,
    what: &str,
) -> std::process::Output {
    const MAX_ATTEMPTS: u32 = 5;
    for attempt in 0..MAX_ATTEMPTS {
        let mut cmd = build();
        isolate_git_config(&mut cmd);
        let out = cmd
            .output()
            .unwrap_or_else(|e| panic!("{what} failed to spawn: {e}"));
        let last = attempt + 1 == MAX_ATTEMPTS;
        if is_transient_spawn_failure(&out.status) && !last {
            // 50, 100, 200, 400ms — give the loader time to shed handle pressure.
            let backoff = std::time::Duration::from_millis(50u64 << attempt);
            std::thread::sleep(backoff);
            continue;
        }
        return out;
    }
    unreachable!("loop returns on the final attempt")
}

/// Run `git <args>` in `dir` with interactive credential prompts disabled,
/// returning the raw [`std::process::Output`].
///
/// The identity, signing and hook settings come from [`isolate_git_config`]
/// (applied by [`output_with_spawn_retry`]); `GIT_TERMINAL_PROMPT=0` is
/// child-only env — never process-global `std::env::set_var`, which would
/// race any parallel test that guards the same `GIT_*` variables.
pub fn git_test_output(dir: &Path, args: &[&str]) -> std::process::Output {
    output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.args(args)
                .current_dir(dir)
                .env("GIT_TERMINAL_PROMPT", "0");
            cmd
        },
        "git",
    )
}

/// [`git_test_output`] variant that asserts the command succeeded.
pub fn git_test_ok(dir: &Path, args: &[&str]) {
    let out = git_test_output(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// [`git_test_output`] variant that asserts success and returns trimmed stdout.
pub fn git_test_stdout(dir: &Path, args: &[&str]) -> String {
    let out = git_test_output(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(cmd: &Command, name: &str) -> Option<String> {
        cmd.get_envs()
            .find(|(key, _)| *key == name)
            .and_then(|(_, value)| value.map(|v| v.to_string_lossy().into_owned()))
    }

    fn git_in(dir: &Path, args: &[&str]) -> std::process::Output {
        git_test_output(dir, args)
    }

    #[test]
    fn a_git_fixture_spawn_reads_the_fixture_config() {
        let mut cmd = Command::new("git");
        cmd.args(["commit", "-m", "x"]);
        isolate_git_config(&mut cmd);
        let dir = fixture_git_dir();
        assert_eq!(
            env_of(&cmd, "GIT_CONFIG_GLOBAL").as_deref(),
            Some(dir.join("gitconfig").to_str().unwrap())
        );
        assert_eq!(
            env_of(&cmd, "GIT_TEMPLATE_DIR").as_deref(),
            Some(dir.join("template").to_str().unwrap())
        );
        assert_eq!(
            env_of(&cmd, "GIT_CONFIG_NOSYSTEM").as_deref(),
            if cfg!(windows) { None } else { Some("1") }
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("gitconfig")).unwrap(),
            FIXTURE_GLOBAL_CONFIG
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("template/config")).unwrap(),
            FIXTURE_REPO_CONFIG
        );
    }

    #[test]
    fn a_builder_that_chose_its_own_config_keeps_it() {
        let mut cmd = Command::new("git");
        cmd.env("GIT_CONFIG_GLOBAL", "/x/gitconfig")
            .env("GIT_CONFIG_SYSTEM", "/x/system")
            .env("GIT_TEMPLATE_DIR", "/x/template");
        isolate_git_config(&mut cmd);
        assert_eq!(
            env_of(&cmd, "GIT_CONFIG_GLOBAL").as_deref(),
            Some("/x/gitconfig")
        );
        assert_eq!(
            env_of(&cmd, "GIT_TEMPLATE_DIR").as_deref(),
            Some("/x/template")
        );
        assert_eq!(env_of(&cmd, "GIT_CONFIG_NOSYSTEM"), None);
    }

    #[test]
    fn a_command_that_is_not_git_is_left_alone() {
        let mut cmd = Command::new("node");
        isolate_git_config(&mut cmd);
        assert_eq!(cmd.get_envs().count(), 0);
    }

    /// `git.exe` and a path to git are git too; `gitk` and `git-lfs` are not.
    #[test]
    fn every_spelling_of_the_git_program_is_isolated() {
        for program in [
            "git",
            "git.exe",
            "/usr/bin/git",
            "C:\\Program Files\\Git\\cmd\\git.exe",
        ] {
            let mut cmd = Command::new(program);
            isolate_git_config(&mut cmd);
            assert!(
                env_of(&cmd, "GIT_CONFIG_GLOBAL").is_some(),
                "{program} must be isolated"
            );
        }
        for program in ["gitk", "git-lfs", "/opt/git/bin/gh"] {
            let mut cmd = Command::new(program);
            isolate_git_config(&mut cmd);
            assert_eq!(cmd.get_envs().count(), 0, "{program} is not git");
        }
    }

    /// Config handed down through the environment is dropped unless the
    /// builder set it itself.
    #[test]
    #[serial_test::serial(git_config_env)]
    fn inherited_environment_config_is_removed_from_the_child() {
        let _lock = crate::test_helpers::env::env_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _count = crate::test_helpers::env::EnvGuard::set("GIT_CONFIG_COUNT", "1");
        let _key = crate::test_helpers::env::EnvGuard::set("GIT_CONFIG_KEY_0", "commit.gpgsign");
        let _value = crate::test_helpers::env::EnvGuard::set("GIT_CONFIG_VALUE_0", "true");
        let _params = crate::test_helpers::env::EnvGuard::set(
            "GIT_CONFIG_PARAMETERS",
            "'commit.gpgsign=true'",
        );

        let mut cmd = Command::new("git");
        isolate_git_config(&mut cmd);
        let removed: Vec<String> = cmd
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();
        for name in [
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_0",
            "GIT_CONFIG_PARAMETERS",
        ] {
            assert!(
                removed.iter().any(|k| k == name),
                "{name} must be removed: {removed:?}"
            );
        }

        let out = output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(["config", "--get", "commit.gpgsign"]);
                cmd.current_dir(std::env::temp_dir());
                cmd
            },
            "git",
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "false",
            "the inherited `commit.gpgsign=true` must not reach the child"
        );

        let mut own = Command::new("git");
        own.env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "x.y")
            .env("GIT_CONFIG_VALUE_0", "z");
        isolate_git_config(&mut own);
        assert_eq!(env_of(&own, "GIT_CONFIG_COUNT").as_deref(), Some("1"));
        assert_eq!(env_of(&own, "GIT_CONFIG_KEY_0").as_deref(), Some("x.y"));
    }

    /// A git spawned the way production spawns it — a plain `Command` with
    /// the inherited environment, no isolation — still commits unsigned and
    /// runs no hooks, because `.cargo/config.toml`'s `[env]` table hands
    /// every cargo-launched process that config. The planted signing global
    /// config is what a developer's host has.
    #[test]
    fn a_production_shaped_git_spawn_signs_nothing_under_cargo() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".gitconfig"),
            "[commit]\n\tgpgsign = true\n[core]\n\thooksPath = /nonexistent\n",
        )
        .unwrap();
        let repo = tempfile::tempdir().unwrap();
        for (key, want) in [
            ("commit.gpgsign", "false"),
            ("tag.gpgsign", "false"),
            ("core.hooksPath", ".git/hooks"),
        ] {
            let mut cmd = Command::new("git"); // spawn-retry-ok: the uninsulated production spawn shape is the subject
            cmd.args(["config", "--get", key])
                .current_dir(repo.path())
                .env("HOME", home.path())
                .env("USERPROFILE", home.path());
            let out = cmd.output().unwrap();
            assert_eq!(
                String::from_utf8_lossy(&out.stdout).trim(),
                want,
                "run under cargo, the [env] table must outrank the host's global config for {key}"
            );
        }
    }

    /// A planted global config under a temp `HOME` (and `XDG_CONFIG_HOME`)
    /// never reaches a fixture spawn, and no config file outside the
    /// repository and the fixture directory is read at all.
    #[test]
    fn the_retry_helper_isolates_the_git_it_spawns() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".gitconfig"),
            "[fixture]\n\tplanted = home\n[commit]\n\tgpgsign = true\n",
        )
        .unwrap();
        let xdg = home.path().join("xdg/git");
        std::fs::create_dir_all(&xdg).unwrap();
        std::fs::write(xdg.join("config"), "[fixture]\n\tplanted = xdg\n").unwrap();
        let repo = tempfile::tempdir().unwrap();
        let home_path = home.path().to_path_buf();
        let out = output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(["config", "--show-origin", "--list"]);
                cmd.current_dir(repo.path());
                cmd.env("HOME", &home_path)
                    .env("USERPROFILE", &home_path)
                    .env("XDG_CONFIG_HOME", home_path.join("xdg"));
                cmd
            },
            "git",
        );
        let listed = String::from_utf8_lossy(&out.stdout);
        assert!(
            !listed.contains("fixture.planted"),
            "a fixture git read a planted global config:\n{listed}"
        );
        assert!(
            listed.contains("commit.gpgsign=false"),
            "the fixture global config must be read:\n{listed}"
        );
        let fixture = fixture_git_dir().to_string_lossy().replace('\\', "/");
        for line in listed.lines() {
            let Some(origin) = line.strip_prefix("file:") else {
                continue;
            };
            let origin = origin
                .split('\t')
                .next()
                .unwrap_or(origin)
                .replace('\\', "/");
            let allowed = origin.starts_with(&fixture)
                || (cfg!(windows) && origin.to_ascii_lowercase().contains("/etc/gitconfig"));
            assert!(
                allowed,
                "a config file outside the fixture was read: {line}\n{listed}"
            );
        }
    }

    /// A repository a fixture creates carries the fixture config at the
    /// repository level, so a git spawned there without the isolation (the
    /// code under test's own spawn) commits unsigned under the fixed identity;
    /// so does a clone of it.
    #[test]
    fn a_fixture_init_and_clone_carry_the_repository_config() {
        let origin = tempfile::tempdir().unwrap();
        assert!(git_in(origin.path(), &["init", "-q"]).status.success());
        let config = std::fs::read_to_string(origin.path().join(".git/config")).unwrap();
        for key in [
            "gpgsign = false",
            "hooksPath = .git/hooks",
            "name = Anodizer Test",
        ] {
            assert!(config.contains(key), "{key} missing from:\n{config}");
        }
        std::fs::write(origin.path().join("f"), "x").unwrap();
        assert!(git_in(origin.path(), &["add", "f"]).status.success());
        // The production shape: a plain git with no isolation at all.
        let committed = Command::new("git")
            .args(["commit", "-qm", "x"])
            .current_dir(origin.path())
            .output()
            .unwrap();
        assert!(
            committed.status.success(),
            "{}",
            String::from_utf8_lossy(&committed.stderr)
        );
        let author = git_in(origin.path(), &["log", "-1", "--format=%an <%ae>"]);
        assert_eq!(
            String::from_utf8_lossy(&author.stdout).trim(),
            "Anodizer Test <test@anodizer.local>"
        );

        let clone = tempfile::tempdir().unwrap();
        let url = origin.path().to_str().unwrap();
        assert!(
            git_in(clone.path(), &["clone", "-q", url, "."])
                .status
                .success()
        );
        let config = std::fs::read_to_string(clone.path().join(".git/config")).unwrap();
        assert!(config.contains("gpgsign = false"), "{config}");
    }

    /// An identity the fixture sets itself outranks the fixed one.
    #[test]
    fn a_repository_identity_outranks_the_fixture_identity() {
        let repo = tempfile::tempdir().unwrap();
        assert!(git_in(repo.path(), &["init", "-q"]).status.success());
        assert!(
            git_in(repo.path(), &["config", "user.name", "Alice"])
                .status
                .success()
        );
        std::fs::write(repo.path().join("f"), "x").unwrap();
        assert!(git_in(repo.path(), &["add", "f"]).status.success());
        assert!(
            git_in(repo.path(), &["commit", "-qm", "x"])
                .status
                .success()
        );
        let author = git_in(repo.path(), &["log", "-1", "--format=%an"]);
        assert_eq!(String::from_utf8_lossy(&author.stdout).trim(), "Alice");
    }
}
