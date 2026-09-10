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

/// Run a freshly-built [`Command`] to completion, retrying on transient Windows
/// process-creation failures.
///
/// `build` MUST construct a brand-new [`Command`] on each call — [`Command`] is
/// consumed by [`Command::output`], so a single instance cannot be reused
/// across attempts. `what` names the spawned tool for panic messages.
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
        let out = build()
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

/// Run `git <args>` in `dir` with a fixed per-invocation test identity and
/// interactive credential prompts disabled, returning the raw
/// [`std::process::Output`].
///
/// The identity is supplied as `-c user.name=` / `-c user.email=` config args
/// (plus `commit.gpgsign=false`) on this one invocation and
/// `GIT_TERMINAL_PROMPT=0` as child-only env — never via process-global
/// `std::env::set_var`, which would race any parallel test that guards the
/// same `GIT_*` variables. Later `-c` args win, so callers can still override
/// per call.
pub fn git_test_output(dir: &Path, args: &[&str]) -> std::process::Output {
    output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.args([
                "-c",
                "user.name=Anodizer Test",
                "-c",
                "user.email=test@anodizer.local",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
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
