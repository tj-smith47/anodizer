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
//! keyed on the cache directory's canonical path and held across a whole
//! keyless run, so two anodizer *processes* on one host queue rather than
//! race. Two points take it: [`keyless_cosign_host_locks`] for a run whose
//! jobs can name several stores (a `TUF_ROOT` templated per artifact or per
//! image), which locks every distinct one in sorted path order, and
//! [`keyless_cosign_host_lock`] for a run with a single config-level env.
//! Advisory locks are released by the OS on process exit, so a killed holder
//! never wedges later runs.

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

/// Take the host-level TUF lock for the one cache directory a config-level
/// env resolves to, for the duration of a keyless cosign run.
///
/// Concurrent keyless cosign invocations on one host collide on the sigstore
/// TUF trust store and the losers exit with `creating cached local store:
/// resource temporarily unavailable`, whether or not the store is already
/// populated. Holding this lock makes a second anodizer process on the same
/// host queue behind the first instead of racing it. A run whose jobs can
/// resolve DIFFERENT stores takes [`keyless_cosign_host_locks`] instead.
///
/// `None` when the cache directory cannot be resolved (no `TUF_ROOT`, no
/// home) or the lock cannot be taken — signing must never fail on lock
/// plumbing, so both degrade to a verbose note and an unserialized run.
pub(crate) fn keyless_cosign_host_lock(
    config_env: &[(String, String)],
    env: &dyn EnvSource,
    log: &StageLogger,
) -> Option<TufInitLock> {
    host_lock_for_dir(&tuf_cache_dir(config_env, env)?, log)
}

/// Take the host-level TUF lock for every distinct cache directory the given
/// per-job envs resolve to.
///
/// A `TUF_ROOT` that renders per artifact (`{{ .Os }}`, `{{ .Target }}`, …)
/// puts a config's jobs on several trust stores; one lock would leave every
/// other store racing a sibling process. Acquisition follows sorted path
/// order — a total order every process agrees on — so two runs holding
/// overlapping sets queue instead of deadlocking. Unresolvable or unlockable
/// roots are skipped exactly as in [`keyless_cosign_host_lock`].
pub(crate) fn keyless_cosign_host_locks(
    job_envs: &[&[(String, String)]],
    env: &dyn EnvSource,
    log: &StageLogger,
) -> Vec<TufInitLock> {
    let roots = distinct_cache_roots(job_envs, env);
    if roots.len() > 1 {
        log.verbose(&format!(
            "keyless jobs resolve {} distinct TUF_ROOT values; locking each: {}",
            roots.len(),
            roots
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    roots
        .iter()
        .filter_map(|root| host_lock_for_dir(root, log))
        .collect()
}

/// The distinct TUF cache directories a set of per-job envs resolves to, in
/// sorted order.
///
/// De-duplication is by CANONICAL path: two spellings of one store
/// (`/x/root` and `/x/sub/../root`, a symlinked cache dir) name the same
/// sentinel file, so treating them as two roots would make one keyless run
/// block on a lock it already holds. A directory that cannot be created or
/// canonicalized keeps its literal spelling — locking it is best-effort
/// anyway.
fn distinct_cache_roots(job_envs: &[&[(String, String)]], env: &dyn EnvSource) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = job_envs
        .iter()
        .filter_map(|job_env| tuf_cache_dir(job_env, env))
        .map(|root| canonical_cache_dir(&root).unwrap_or(root))
        .collect();
    roots.sort();
    roots.dedup();
    roots
}

/// Create the TUF cache directory (and parents) if needed and resolve it to
/// its canonical form.
fn canonical_cache_dir(cache_dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(cache_dir)
        .with_context(|| format!("creating sigstore TUF cache dir {}", cache_dir.display()))?;
    std::fs::canonicalize(cache_dir)
        .with_context(|| format!("resolving sigstore TUF cache dir {}", cache_dir.display()))
}

/// Lock one resolved TUF cache directory, probing before blocking.
fn host_lock_for_dir(dir: &Path, log: &StageLogger) -> Option<TufInitLock> {
    let (file, sentinel) = match TufInitLock::open_sentinel(dir) {
        Ok(opened) => opened,
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
        return Some(TufInitLock { file, sentinel }.noted(log));
    }
    log.status("waiting for the host-level TUF lock held by another anodizer process on this host"); // status-ok: explains a multi-minute stall
    match fs4::FileExt::lock(&file) {
        Ok(()) => Some(TufInitLock { file, sentinel }.noted(log)),
        Err(err) => {
            log.verbose(&format!(
                "could not acquire host-level TUF init lock on {} ({err:#}); \
                 keyless cosign runs unserialized across processes",
                sentinel.display()
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
    /// The sentinel actually opened — under the CANONICAL cache dir, which
    /// is why every message about this lock is formatted from here and never
    /// from the spelling the caller passed in.
    sentinel: PathBuf,
}

impl TufInitLock {
    /// Create the cache directory (and parents) if needed and open the
    /// sentinel file inside it, unlocked.
    ///
    /// The sentinel is keyed on the directory's CANONICAL path, so two runs
    /// spelling one store differently contend the same file.
    fn open_sentinel(cache_dir: &Path) -> Result<(File, PathBuf)> {
        let cache_dir = canonical_cache_dir(cache_dir)?;
        let path = cache_dir.join(LOCK_SENTINEL);
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening TUF init lock sentinel {}", path.display()))?;
        Ok((file, path))
    }

    /// Create the cache directory (and parents) if needed, then take an
    /// exclusive blocking lock on the sentinel file inside it, with no
    /// non-blocking probe first — production instead reaches
    /// [`host_lock_for_dir`], which probes and explains the wait.
    #[cfg(test)]
    pub(crate) fn acquire(cache_dir: &Path) -> Result<Self> {
        let (file, sentinel) = Self::open_sentinel(cache_dir)?;
        // Explicit trait call: on toolchains ≥1.89 `std::fs::File` grew an
        // inherent `lock` that would otherwise shadow the fs4 method; on the
        // 1.87 MSRV only the fs4 method exists.
        fs4::FileExt::lock(&file)
            .with_context(|| format!("locking TUF init sentinel {}", sentinel.display()))?;
        Ok(Self { file, sentinel })
    }

    /// The sentinel file this lock holds, under the canonical cache dir.
    pub(crate) fn sentinel(&self) -> &Path {
        &self.sentinel
    }

    /// Leave a verbose trace of which sentinel this run holds, so a stall
    /// reported against one store can be matched to the process holding it.
    fn noted(self, log: &StageLogger) -> Self {
        log.verbose(&format!(
            "holding the host-level TUF init lock {}",
            self.sentinel().display()
        ));
        self
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

    /// Two spellings of one store name one sentinel file, so treating them
    /// as two roots would make a single run block on a lock it already
    /// holds. They must collapse to one canonical root, and one lock.
    #[test]
    fn spellings_of_one_cache_dir_resolve_to_one_root() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
        let plain = vec![("TUF_ROOT".to_string(), root.display().to_string())];
        // `..` survives `Path` component normalization, so only a real
        // canonicalization can collapse this onto `root`.
        let indirect = vec![(
            "TUF_ROOT".to_string(),
            tmp.path()
                .join("sub")
                .join("..")
                .join("root")
                .display()
                .to_string(),
        )];
        let env = MapEnvSource::new();

        let roots = distinct_cache_roots(&[&plain, &indirect], &env);
        assert_eq!(
            roots.len(),
            1,
            "one store spelled two ways must be one root: {roots:?}"
        );
        assert_eq!(roots[0], std::fs::canonicalize(&root).unwrap());

        let log = StageLogger::new("sign", anodizer_core::log::Verbosity::Normal);
        let locks = keyless_cosign_host_locks(&[&plain, &indirect], &env, &log);
        assert_eq!(locks.len(), 1, "one root, one lock");
        assert!(root.join(LOCK_SENTINEL).is_file());
    }

    /// The singular point keys its sentinel on the canonical cache dir: a
    /// child env spelling the store as `<tmp>/sub/../root` holds the lock
    /// under `<tmp>/root`, so a sibling process naming the store plainly
    /// contends the same file.
    #[test]
    fn singular_lock_keys_the_sentinel_on_the_canonical_root() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
        let spelled = tmp.path().join("sub").join("..").join("root");
        let config_env = vec![("TUF_ROOT".to_string(), spelled.display().to_string())];
        let log = StageLogger::new("sign", anodizer_core::log::Verbosity::Normal);

        let lock = keyless_cosign_host_lock(&config_env, &MapEnvSource::new(), &log)
            .expect("the spelled root resolves and locks");
        let canonical_sentinel = std::fs::canonicalize(&root).unwrap().join(LOCK_SENTINEL);
        assert_eq!(lock.sentinel(), canonical_sentinel);
        assert!(canonical_sentinel.is_file());
        assert!(!lock.sentinel().components().any(|c| c.as_os_str() == ".."));
    }

    /// Every message about the lock names the sentinel that was actually
    /// opened — under the canonical cache dir — never the caller's spelling.
    /// A directory squatting on the sentinel path is the one open failure
    /// reachable as root; the guard's own path covers the locked case.
    #[test]
    fn lock_failure_names_the_canonical_sentinel() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
        let spelled = tmp.path().join("sub").join("..").join("root");
        let canonical_sentinel = {
            std::fs::create_dir_all(&root).unwrap();
            std::fs::canonicalize(&root).unwrap().join(LOCK_SENTINEL)
        };

        let lock = TufInitLock::acquire(&spelled).expect("acquire through the indirect spelling");
        assert_eq!(lock.sentinel(), canonical_sentinel);
        drop(lock);

        std::fs::remove_file(&canonical_sentinel).unwrap();
        std::fs::create_dir(&canonical_sentinel).unwrap();
        let msg = match TufInitLock::acquire(&spelled) {
            Ok(_) => panic!("a directory on the sentinel path must fail the acquire"),
            Err(err) => format!("{err:#}"),
        };
        assert!(
            msg.contains(&canonical_sentinel.display().to_string()),
            "message must name the canonical sentinel: {msg}"
        );
        assert!(
            !msg.contains(".."),
            "message must not carry the caller's spelling: {msg}"
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
            // If the lock excluded this thread, the holder cleared the flag first.
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
