//! Host-level coordination for keyless cosign's sigstore TUF trust store.
//!
//! Keyless cosign reads — and lazily initializes — a TUF trust-root cache
//! per host (default `~/.sigstore/root`, overridable via `TUF_ROOT`).
//! Concurrent keyless cosign invocations on one host collide on that store
//! and the losers fail with `creating cached local store: resource
//! temporarily unavailable`. A populated store does not make the collision
//! go away: a read-only `cosign verify-blob` has been observed failing that
//! way while sibling workers were live, under a second after a completed
//! initialization.
//!
//! The host-side guard is therefore [`TufInitLock`] — an advisory file lock
//! scoped to the cache directory and held across a whole keyless run, so two
//! anodizer *processes* on one host queue rather than race.
//! [`keyless_cosign_host_lock`] is the seam every keyless spawn site takes it
//! through. Advisory locks are released by the OS on process exit, so a
//! killed holder never wedges later runs.

use std::fs::File;
use std::path::{Path, PathBuf};

use anodizer_core::env_source::EnvSource;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};

/// Lock-sentinel filename created inside the TUF cache directory. cosign
/// tolerates unrelated entries in the cache dir (it `MkdirAll`s and reads
/// only its own files), so co-locating the sentinel keeps the lock scoped to
/// the exact cache a `TUF_ROOT` override points at.
const LOCK_SENTINEL: &str = ".anodizer-tuf-init.lock";

/// Read `name` from the layered environment the cosign child will actually
/// see: the sign config's rendered `env:` entries overlay the anodizer
/// process env (the child inherits the process env plus those entries, with
/// the last duplicate entry winning, matching `Command::envs`).
fn layered_var(name: &str, config_env: &[(String, String)], env: &dyn EnvSource) -> Option<String> {
    config_env
        .iter()
        .rev()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.clone())
        .or_else(|| env.var(name))
}

/// Resolve the TUF trust-root cache directory keyless cosign will use.
///
/// Mirrors sigstore's `pkg/tuf` resolution against the CHILD's environment:
/// a `TUF_ROOT` set in the sign config's rendered `env:` entries
/// (`config_env`) shadows the process env; otherwise `<home>/.sigstore/root`,
/// where home is `$HOME` on Unix and `%USERPROFILE%` on Windows (Go's
/// `os.UserHomeDir`), also subject to the overlay. Returns `None` when
/// neither is available — callers then fall back to process-local
/// serialization only.
pub(crate) fn tuf_cache_dir(
    config_env: &[(String, String)],
    env: &dyn EnvSource,
) -> Option<PathBuf> {
    if let Some(root) = layered_var("TUF_ROOT", config_env, env).filter(|v| !v.trim().is_empty()) {
        return Some(PathBuf::from(root));
    }
    let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let home = layered_var(home_var, config_env, env).filter(|v| !v.trim().is_empty())?;
    Some(Path::new(&home).join(".sigstore").join("root"))
}

/// Take the host-level TUF lock for the duration of a keyless cosign run.
///
/// Concurrent keyless cosign invocations on one host collide on the sigstore
/// TUF trust store and the losers exit with `creating cached local store:
/// resource temporarily unavailable`, whether or not the store is already
/// populated. Every keyless spawn site takes this lock so a second anodizer
/// process on the same host queues behind the first instead of racing it.
///
/// `None` when the cache directory cannot be resolved (no `TUF_ROOT`, no
/// home) or the lock cannot be taken — signing must never fail on lock
/// plumbing, so both degrade to a verbose note and an unserialized run.
pub(crate) fn keyless_cosign_host_lock(
    config_env: &[(String, String)],
    env: &dyn EnvSource,
    log: &StageLogger,
) -> Option<TufInitLock> {
    let dir = tuf_cache_dir(config_env, env)?;
    let file = match TufInitLock::open_sentinel(&dir) {
        Ok(file) => file,
        Err(err) => {
            log.verbose(&format!(
                "could not acquire host-level TUF init lock ({err:#}); \
                 keyless cosign runs unserialized across processes"
            ));
            return None;
        }
    };
    // Probe before blocking: waiting here can stall a release for however
    // long the other process signs, and an unexplained stall gets reported
    // as a hang.
    if fs4::FileExt::try_lock(&file).is_ok() {
        return Some(TufInitLock { file });
    }
    log.status("waiting for the host-level TUF lock held by another anodizer process on this host"); // status-ok: explains a multi-minute stall
    match fs4::FileExt::lock(&file) {
        Ok(()) => Some(TufInitLock { file }),
        Err(err) => {
            log.verbose(&format!(
                "could not acquire host-level TUF init lock ({err:#}); \
                 keyless cosign runs unserialized across processes"
            ));
            None
        }
    }
}

/// RAII exclusive advisory lock on the TUF cache's init sentinel.
///
/// Blocks until the lock is granted. Unlocked on drop; because the lock is
/// advisory (flock / `LockFileEx`), the OS also releases it if the process
/// dies while holding it.
pub(crate) struct TufInitLock {
    file: File,
}

impl TufInitLock {
    /// Create the cache directory (and parents) if needed and open the
    /// sentinel file inside it, unlocked.
    fn open_sentinel(cache_dir: &Path) -> Result<File> {
        std::fs::create_dir_all(cache_dir)
            .with_context(|| format!("creating sigstore TUF cache dir {}", cache_dir.display()))?;
        let path = cache_dir.join(LOCK_SENTINEL);
        File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening TUF init lock sentinel {}", path.display()))
    }

    /// Create the cache directory (and parents) if needed, then take an
    /// exclusive blocking lock on the sentinel file inside it. Production
    /// goes through [`keyless_cosign_host_lock`], which probes first.
    #[cfg(test)]
    pub(crate) fn acquire(cache_dir: &Path) -> Result<Self> {
        let file = Self::open_sentinel(cache_dir)?;
        // Explicit trait call: on toolchains ≥1.89 `std::fs::File` grew an
        // inherent `lock` that would otherwise shadow the fs4 method; on the
        // 1.87 MSRV only the fs4 method exists.
        fs4::FileExt::lock(&file).with_context(|| {
            format!(
                "locking TUF init sentinel {}",
                cache_dir.join(LOCK_SENTINEL).display()
            )
        })?;
        Ok(Self { file })
    }
}

impl Drop for TufInitLock {
    fn drop(&mut self) {
        // Best-effort: the OS releases the lock on fd close / process exit
        // regardless, so an unlock error is not worth surfacing.
        let _ = fs4::FileExt::unlock(&self.file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::MapEnvSource;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[test]
    fn cache_dir_prefers_tuf_root_override() {
        let env = MapEnvSource::new()
            .with("TUF_ROOT", "/custom/tuf")
            .with("HOME", "/home/u")
            .with("USERPROFILE", r"C:\Users\u");
        assert_eq!(
            tuf_cache_dir(&[], &env),
            Some(PathBuf::from("/custom/tuf")),
            "TUF_ROOT must win over the home-derived default"
        );
    }

    #[test]
    fn cache_dir_defaults_under_home_sigstore_root() {
        let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let env = MapEnvSource::new().with(home_var, "/home/u");
        assert_eq!(
            tuf_cache_dir(&[], &env),
            Some(Path::new("/home/u").join(".sigstore").join("root"))
        );
    }

    #[test]
    fn cache_dir_unresolvable_without_home_or_override() {
        assert_eq!(tuf_cache_dir(&[], &MapEnvSource::new()), None);
        // A blank override must not produce an empty path.
        let env = MapEnvSource::new().with("TUF_ROOT", "  ");
        assert_eq!(tuf_cache_dir(&[], &env), None);
    }

    #[test]
    fn cache_dir_layers_config_env_over_process_env_over_home() {
        let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let process = MapEnvSource::new()
            .with("TUF_ROOT", "/process/tuf")
            .with(home_var, "/home/u");

        // Config TUF_ROOT beats process TUF_ROOT (the child sees the config
        // value); the last duplicate config entry wins, like Command::envs.
        let config = vec![
            ("TUF_ROOT".to_string(), "/shadowed".to_string()),
            ("TUF_ROOT".to_string(), "/config/tuf".to_string()),
        ];
        assert_eq!(
            tuf_cache_dir(&config, &process),
            Some(PathBuf::from("/config/tuf"))
        );

        // No config entry: process TUF_ROOT beats the home-derived default.
        assert_eq!(
            tuf_cache_dir(&[], &process),
            Some(PathBuf::from("/process/tuf"))
        );

        // Neither layer sets TUF_ROOT: home-derived default; a config HOME
        // override shadows the process home too.
        let no_root = MapEnvSource::new().with(home_var, "/home/u");
        assert_eq!(
            tuf_cache_dir(&[], &no_root),
            Some(Path::new("/home/u").join(".sigstore").join("root"))
        );
        let config_home = vec![(home_var.to_string(), "/other/home".to_string())];
        assert_eq!(
            tuf_cache_dir(&config_home, &no_root),
            Some(Path::new("/other/home").join(".sigstore").join("root"))
        );

        // A blank config TUF_ROOT shadows the process value in the child's
        // env, so resolution falls through to home — not to /process/tuf.
        let blank = vec![("TUF_ROOT".to_string(), String::new())];
        assert_eq!(
            tuf_cache_dir(&blank, &process),
            Some(Path::new("/home/u").join(".sigstore").join("root"))
        );
    }

    #[test]
    fn lock_excludes_second_locker_until_dropped() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = tmp.path().join("root");

        let holder = TufInitLock::acquire(&cache).expect("first acquire");
        let in_critical = Arc::new(AtomicBool::new(true));

        let cache2 = cache.clone();
        let flag = Arc::clone(&in_critical);
        let contender = std::thread::spawn(move || {
            let _second = TufInitLock::acquire(&cache2).expect("second acquire");
            // If the lock excluded us, the holder cleared the flag before drop.
            assert!(
                !flag.load(Ordering::SeqCst),
                "second locker entered while the first still held the lock"
            );
        });

        // Give the contender ample time to block on the lock.
        std::thread::sleep(Duration::from_millis(300));
        in_critical.store(false, Ordering::SeqCst);
        drop(holder);
        contender.join().expect("contender thread");
    }

    #[test]
    fn lock_released_on_drop() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = tmp.path().join("root");
        drop(TufInitLock::acquire(&cache).expect("first acquire"));

        // A fresh handle must be grantable immediately (non-blocking probe so
        // a regression hangs the try, not the test).
        let sentinel = File::options()
            .write(true)
            .open(cache.join(LOCK_SENTINEL))
            .unwrap();
        fs4::FileExt::try_lock(&sentinel).expect("lock must be free after drop");
        fs4::FileExt::unlock(&sentinel).unwrap();
    }

    #[test]
    fn lock_released_when_holder_panics() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = tmp.path().join("root");
        let cache2 = cache.clone();
        let result = std::panic::catch_unwind(move || {
            let _lock = TufInitLock::acquire(&cache2).expect("acquire before panic");
            panic!("holder dies mid-critical-section");
        });
        assert!(result.is_err(), "closure must have panicked");

        let sentinel = File::options()
            .write(true)
            .open(cache.join(LOCK_SENTINEL))
            .unwrap();
        fs4::FileExt::try_lock(&sentinel).expect("unwind must run Drop and release the lock");
        fs4::FileExt::unlock(&sentinel).unwrap();
    }
}
