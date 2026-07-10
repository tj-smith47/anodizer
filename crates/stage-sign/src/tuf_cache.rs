//! Host-level coordination for cosign's lazy sigstore TUF trust-root init.
//!
//! Keyless cosign initializes a TUF trust-root cache on its first run per
//! host (default `~/.sigstore/root`, overridable via `TUF_ROOT`). Concurrent
//! cold initializations contend cosign's internal flock and the losers fail
//! with `creating cached local store: resource temporarily unavailable`.
//! This module supplies the two host-side guards around that first run:
//!
//! - [`tuf_cache_is_warm`] — detect an already-initialized, still-fresh
//!   cache so a warm host skips the serialized warm-up entirely. Two cache
//!   layouts are recognized: cosign v2.4.3's legacy go-tuf store
//!   (`remote.json` + `targets/` + a LevelDB at `tuf.db/`, as observed on a
//!   real `cosign initialize`) and the sigstore-go JSON store (top-level
//!   `root.json`/`timestamp.json` metadata next to `targets/`);
//! - [`TufInitLock`] — an advisory file lock scoped to the cache directory,
//!   held across the initializing invocation so two anodizer *processes* on
//!   one host cannot both drive a cold init. Advisory locks are released by
//!   the OS on process exit, so a killed holder never wedges later runs.

use std::fs::File;
use std::path::{Path, PathBuf};

use anodizer_core::env_source::EnvSource;
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

/// True when the TUF cache at `dir` is populated enough that cosign's init
/// is a no-op. Two on-disk layouts are recognized, both requiring a
/// non-empty `targets/` directory plus fresh metadata:
///
/// - **JSON layout** (sigstore-go): top-level `root.json`, `snapshot.json`,
///   `targets.json`, `timestamp.json` next to `targets/`. Warm iff
///   `root.json` is a file and `timestamp.json` carries an unexpired
///   `signed.expires`.
/// - **Legacy go-tuf layout** (empirically what cosign v2.4.3 writes:
///   `remote.json`, `targets/`, and a LevelDB at `tuf.db/`): warm iff
///   `tuf.db/CURRENT` is a file and the newest mtime directly under
///   `tuf.db/` is within 24 hours (see [`tuf_db_recently_refreshed`]).
///
/// Conservative on purpose — a partially-written cache, missing metadata, or
/// expired/unparseable/stale metadata all classify COLD, so the run warms up
/// serially rather than fanning out onto a store cosign will (re-)initialize
/// or refresh under its internal flock. The cost asymmetry drives every
/// tie-break: a fresh cache mis-classified cold merely re-serializes one
/// sign, while a stale cache mis-classified warm re-races the refresh lock.
pub(crate) fn tuf_cache_is_warm(dir: &Path) -> bool {
    let has_target = match std::fs::read_dir(dir.join("targets")) {
        Ok(mut entries) => entries.next().is_some(),
        Err(_) => false,
    };
    if !has_target {
        return false;
    }
    if dir.join("root.json").is_file() && timestamp_is_fresh(&dir.join("timestamp.json")) {
        return true;
    }
    dir.join("tuf.db").join("CURRENT").is_file() && tuf_db_recently_refreshed(&dir.join("tuf.db"))
}

/// True when the newest mtime among the entries directly under the go-tuf
/// LevelDB directory `db_dir` is within 24 hours.
///
/// In cosign's legacy go-tuf layout the metadata — including its expiry —
/// lives inside the LevelDB, snappy-compressed and unreadable without a db
/// dependency, so the newest `tuf.db/` mtime (the last successful metadata
/// refresh) stands in for it. The live sigstore timestamp validity window is
/// 7 days, so 24 hours guarantees freshness with wide margin; the asymmetry
/// makes the tight window cheap (stale-classified-cold re-serializes one
/// sign, stale-classified-warm re-races the refresh lock). Any read/metadata
/// error, or an empty `tuf.db/`, classifies COLD.
fn tuf_db_recently_refreshed(db_dir: &Path) -> bool {
    const FRESH_WINDOW: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
    let Ok(entries) = std::fs::read_dir(db_dir) else {
        return false;
    };
    let mut newest: Option<std::time::SystemTime> = None;
    for entry in entries {
        let Ok(entry) = entry else {
            return false;
        };
        let Ok(meta) = entry.metadata() else {
            return false;
        };
        let Ok(mtime) = meta.modified() else {
            return false;
        };
        newest = Some(newest.map_or(mtime, |n| n.max(mtime)));
    }
    let Some(newest) = newest else {
        return false;
    };
    match std::time::SystemTime::now().duration_since(newest) {
        Ok(age) => age <= FRESH_WINDOW,
        // An mtime ahead of the clock means the refresh just happened (or
        // minor skew) — fresh either way.
        Err(_) => true,
    }
}

/// True when the TUF `timestamp.json` at `path` exists, parses, and its
/// `signed.expires` (RFC 3339) lies in the future.
///
/// This covers the sigstore-go JSON cache layout, where the top-level
/// metadata files (`root.json`, `snapshot.json`, `targets.json`,
/// `timestamp.json`) sit directly in the cache root next to `targets/`.
/// Timestamp metadata carries the shortest validity window, so an expired
/// one means cosign will refresh the store through the same locked path as
/// a cold init. No signature verification here — this is a freshness
/// heuristic, not a trust decision (cosign re-verifies).
fn timestamp_is_fresh(path: &Path) -> bool {
    let Ok(raw) = std::fs::read(path) else {
        return false;
    };
    let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&raw) else {
        return false;
    };
    let Some(expires) = doc.pointer("/signed/expires").and_then(|v| v.as_str()) else {
        return false;
    };
    let Ok(expires) = chrono::DateTime::parse_from_rfc3339(expires) else {
        return false;
    };
    expires > chrono::Utc::now()
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
    /// Create the cache directory (and parents) if needed, then take an
    /// exclusive blocking lock on the sentinel file inside it.
    pub(crate) fn acquire(cache_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(cache_dir)
            .with_context(|| format!("creating sigstore TUF cache dir {}", cache_dir.display()))?;
        let path = cache_dir.join(LOCK_SENTINEL);
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening TUF init lock sentinel {}", path.display()))?;
        // Explicit trait call: on toolchains ≥1.89 `std::fs::File` grew an
        // inherent `lock` that would otherwise shadow the fs4 method; on the
        // 1.87 MSRV only the fs4 method exists.
        fs4::FileExt::lock(&file)
            .with_context(|| format!("locking TUF init sentinel {}", path.display()))?;
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

    /// Write a `timestamp.json` whose `signed.expires` is `offset` from now.
    fn write_timestamp(cache: &Path, offset: chrono::Duration) {
        let expires = (chrono::Utc::now() + offset).to_rfc3339();
        std::fs::write(
            cache.join("timestamp.json"),
            format!(r#"{{"signed":{{"expires":"{expires}"}}}}"#),
        )
        .unwrap();
    }

    #[test]
    fn warm_detection_requires_root_json_and_nonempty_targets() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = tmp.path().join("root");

        assert!(!tuf_cache_is_warm(&cache), "missing dir is cold");

        std::fs::create_dir_all(&cache).unwrap();
        assert!(!tuf_cache_is_warm(&cache), "empty dir is cold");

        std::fs::write(cache.join("root.json"), "{}").unwrap();
        assert!(
            !tuf_cache_is_warm(&cache),
            "root.json without targets is cold"
        );

        std::fs::create_dir(cache.join("targets")).unwrap();
        assert!(
            !tuf_cache_is_warm(&cache),
            "empty targets dir is still cold (no fetched target)"
        );

        std::fs::write(cache.join("targets").join("rekor.pub"), "key").unwrap();
        assert!(
            !tuf_cache_is_warm(&cache),
            "populated cache without timestamp.json is cold"
        );

        write_timestamp(&cache, chrono::Duration::hours(1));
        assert!(
            tuf_cache_is_warm(&cache),
            "root.json + fetched target + unexpired timestamp is warm"
        );
    }

    #[test]
    fn warm_detection_rejects_expired_or_unparseable_timestamp() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = tmp.path().join("root");
        std::fs::create_dir_all(cache.join("targets")).unwrap();
        std::fs::write(cache.join("root.json"), "{}").unwrap();
        std::fs::write(cache.join("targets").join("rekor.pub"), "key").unwrap();

        write_timestamp(&cache, chrono::Duration::hours(-1));
        assert!(!tuf_cache_is_warm(&cache), "expired timestamp is cold");

        std::fs::write(cache.join("timestamp.json"), "not json").unwrap();
        assert!(!tuf_cache_is_warm(&cache), "unparseable timestamp is cold");

        std::fs::write(cache.join("timestamp.json"), r#"{"signed":{}}"#).unwrap();
        assert!(
            !tuf_cache_is_warm(&cache),
            "timestamp without signed.expires is cold"
        );

        std::fs::write(
            cache.join("timestamp.json"),
            r#"{"signed":{"expires":"tomorrow-ish"}}"#,
        )
        .unwrap();
        assert!(!tuf_cache_is_warm(&cache), "non-RFC3339 expires is cold");

        write_timestamp(&cache, chrono::Duration::hours(1));
        assert!(tuf_cache_is_warm(&cache), "fresh timestamp restores warm");
    }

    /// Lay down cosign v2.4.3's legacy go-tuf store shape: `remote.json`,
    /// a fetched target, and a LevelDB-ish `tuf.db/` with CURRENT + MANIFEST.
    fn populate_legacy_cache(cache: &Path) {
        std::fs::create_dir_all(cache.join("targets")).unwrap();
        std::fs::create_dir_all(cache.join("tuf.db")).unwrap();
        std::fs::write(cache.join("remote.json"), "{}").unwrap();
        std::fs::write(cache.join("targets").join("fulcio.crt.pem"), "cert").unwrap();
        std::fs::write(cache.join("tuf.db").join("CURRENT"), "MANIFEST-000001\n").unwrap();
        std::fs::write(cache.join("tuf.db").join("MANIFEST-000001"), "m").unwrap();
    }

    /// Backdate every entry directly under `tuf.db/` by `hours`.
    fn age_tuf_db(cache: &Path, hours: u64) {
        let stale = std::time::SystemTime::now() - std::time::Duration::from_secs(hours * 3600);
        for entry in std::fs::read_dir(cache.join("tuf.db")).unwrap() {
            let file = File::options()
                .write(true)
                .open(entry.unwrap().path())
                .unwrap();
            file.set_modified(stale).unwrap();
        }
    }

    #[test]
    fn warm_detection_accepts_fresh_legacy_tuf_db_layout() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = tmp.path().join("root");
        populate_legacy_cache(&cache);
        assert!(
            tuf_cache_is_warm(&cache),
            "tuf.db/CURRENT + fresh mtimes + fetched target is warm"
        );
    }

    #[test]
    fn warm_detection_rejects_stale_legacy_tuf_db() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = tmp.path().join("root");
        populate_legacy_cache(&cache);
        age_tuf_db(&cache, 25);
        assert!(
            !tuf_cache_is_warm(&cache),
            "tuf.db older than 24h is cold (metadata may be expired)"
        );

        // One fresh entry (a refresh touched the db) restores warm.
        std::fs::write(cache.join("tuf.db").join("000002.ldb"), "l").unwrap();
        assert!(
            tuf_cache_is_warm(&cache),
            "newest tuf.db entry within 24h is warm again"
        );
    }

    #[test]
    fn warm_detection_rejects_incomplete_legacy_layout() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = tmp.path().join("root");
        populate_legacy_cache(&cache);

        std::fs::remove_file(cache.join("targets").join("fulcio.crt.pem")).unwrap();
        assert!(
            !tuf_cache_is_warm(&cache),
            "legacy layout with empty targets is cold"
        );
        std::fs::write(cache.join("targets").join("fulcio.crt.pem"), "cert").unwrap();

        std::fs::remove_file(cache.join("tuf.db").join("CURRENT")).unwrap();
        assert!(
            !tuf_cache_is_warm(&cache),
            "tuf.db without CURRENT is cold (LevelDB never finished a write)"
        );
    }

    #[test]
    fn warm_detection_rejects_root_json_directory() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = tmp.path().join("root");
        std::fs::create_dir_all(cache.join("root.json")).unwrap();
        std::fs::create_dir_all(cache.join("targets")).unwrap();
        std::fs::write(cache.join("targets").join("t"), "x").unwrap();
        write_timestamp(&cache, chrono::Duration::hours(1));
        assert!(!tuf_cache_is_warm(&cache), "root.json must be a file");
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
