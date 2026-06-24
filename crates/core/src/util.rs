use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result};

/// Compile a regex, panicking with a diagnostic if the pattern is invalid.
/// Intended for `LazyLock::new(…)` initializers where the pattern is a
/// hardcoded literal (or built from `format!` over known-safe fragments).
/// A compile failure means a programmer bug surfaced at first use, not a
/// runtime-path user-input error. Exists because the anti-pattern hook
/// forbids bare panicking error helpers in lib code, and `Regex::new` on
/// a trusted literal is inherently infallible.
pub fn static_regex(pattern: &str) -> regex::Regex {
    regex::Regex::new(pattern)
        .unwrap_or_else(|e| panic!("invalid static regex literal `{}`: {}", pattern, e))
}

// ---------------------------------------------------------------------------
// Topological sort (Kahn's algorithm)
// ---------------------------------------------------------------------------

/// Topologically sort items by their dependency lists.
///
/// Input: slice of `(name, depends_on)` pairs.
/// Output: names in dependency order (dependencies before dependents).
///
/// - Dependencies that are not in the input set are silently ignored.
/// - Deterministic: zero-in-degree nodes are sorted alphabetically.
/// - On cycles: sorted nodes are returned followed by remaining nodes in
///   their original order.
pub fn topological_sort(items: &[(impl AsRef<str>, impl AsRef<[String]>)]) -> Vec<String> {
    let names: HashSet<&str> = items.iter().map(|(n, _)| n.as_ref()).collect();

    let mut in_degree: HashMap<&str, usize> = items
        .iter()
        .map(|(n, deps)| {
            let deg = deps
                .as_ref()
                .iter()
                .filter(|d| names.contains(d.as_str()))
                .count();
            (n.as_ref(), deg)
        })
        .collect();

    // edges: dep → list of dependents
    let mut edges: HashMap<&str, Vec<&str>> = HashMap::new();
    for (n, deps) in items {
        for dep in deps.as_ref() {
            if names.contains(dep.as_str()) {
                edges.entry(dep.as_str()).or_default().push(n.as_ref());
            }
        }
    }

    // Kahn's algorithm with deterministic seed ordering
    let mut queue: VecDeque<&str> = {
        let mut v: Vec<&str> = in_degree
            .iter()
            .filter(|(_, d)| **d == 0)
            .map(|(&n, _)| n)
            .collect();
        v.sort_unstable();
        VecDeque::from(v)
    };

    let mut result = Vec::with_capacity(items.len());
    while let Some(node) = queue.pop_front() {
        result.push(node.to_string());
        if let Some(dependents) = edges.get(node) {
            let mut next: Vec<&str> = dependents
                .iter()
                .filter_map(|&dep| {
                    let deg = in_degree.get_mut(dep)?;
                    *deg -= 1;
                    if *deg == 0 { Some(dep) } else { None }
                })
                .collect();
            next.sort_unstable();
            for n in next {
                queue.push_back(n);
            }
        }
    }

    // Append remaining (cycle case) in original order.
    if result.len() < items.len() {
        let in_result: HashSet<String> = result.iter().cloned().collect();
        for (n, _) in items {
            if !in_result.contains(n.as_ref()) {
                result.push(n.as_ref().to_string());
            }
        }
    }

    result
}

// ---------------------------------------------------------------------------
// find_binary
// ---------------------------------------------------------------------------

/// Check whether a binary can be found on the system.
///
/// For absolute or relative paths (containing `/`), checks if the file exists.
/// For bare names, searches each directory in the `PATH` environment variable
/// for an executable with the given name. This is a pure-Rust implementation
/// that avoids shelling out to `which` or `command -v`, making it portable
/// across all platforms.
pub fn find_binary(name: &str) -> bool {
    if name.contains('/') || name.contains('\\') {
        return Path::new(name).exists();
    }

    // On Windows, PATHEXT lists extensions to try (e.g., .COM;.EXE;.BAT;.CMD).
    // When the caller asks for "upx", we also check for "upx.exe", etc.
    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .filter(|e| !e.is_empty())
            .map(|e| e.to_string())
            .collect()
    } else {
        Vec::new()
    };

    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return true;
            }
            for ext in &extensions {
                let with_ext = dir.join(format!("{}{}", name, ext));
                if with_ext.is_file() {
                    return true;
                }
            }
        }
    }

    false
}

// ---------------------------------------------------------------------------
// apply_mod_timestamp
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// mod_timestamp helpers
// ---------------------------------------------------------------------------

/// Parse a `mod_timestamp` string into a `SystemTime`.
///
/// Accepts:
///   - Unix epoch seconds as an integer (e.g. `"1704067200"`)
///   - RFC 3339 / ISO 8601 datetime (e.g. `"2024-01-01T00:00:00Z"`)
pub fn parse_mod_timestamp(raw: &str) -> Result<SystemTime> {
    // Try Unix epoch integer first (most common in CI)
    if let Ok(epoch_secs) = raw.parse::<u64>() {
        return Ok(SystemTime::UNIX_EPOCH + Duration::from_secs(epoch_secs));
    }
    // Try RFC 3339 / ISO 8601 via chrono
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        let epoch_secs = dt.timestamp() as u64;
        return Ok(SystemTime::UNIX_EPOCH + Duration::from_secs(epoch_secs));
    }
    // Try chrono's more lenient parsing for formats like "2024-01-01T00:00:00"
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S") {
        let epoch_secs = dt.and_utc().timestamp() as u64;
        return Ok(SystemTime::UNIX_EPOCH + Duration::from_secs(epoch_secs));
    }
    anyhow::bail!(
        "mod_timestamp value '{raw}' is not a valid timestamp. \
         Accepted formats: Unix epoch seconds (e.g. \"1704067200\") or \
         RFC 3339 datetime (e.g. \"2024-01-01T00:00:00Z\")"
    )
}

/// Apply `mod_timestamp` to every regular file in a directory tree.
///
/// Parses the timestamp via `parse_mod_timestamp`, then recurses into
/// subdirectories, setting the mtime on every regular file. Symlinks are not
/// followed and directory mtimes are left untouched (files-only semantics,
/// matching [`pin_dir_mtimes_epoch`], the SDE reproducibility floor this
/// override is layered on top of). A nested staged file — e.g. a
/// `templated_extra_files` entry whose dst is `docs/README.txt` — must receive
/// the user's `mod_timestamp`, not the SDE epoch left by the floor.
pub fn apply_mod_timestamp(dir: &Path, raw: &str, log: &crate::log::StageLogger) -> Result<()> {
    let mtime = parse_mod_timestamp(raw)?;

    let mut stack: Vec<std::path::PathBuf> = vec![dir.to_path_buf()];
    while let Some(p) = stack.pop() {
        for entry in
            fs::read_dir(&p).with_context(|| format!("read staging dir {}", p.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let ft = entry.file_type()?;
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                set_file_mtime(&path, mtime)?;
            }
        }
    }

    log.status(&format!("applied mod_timestamp={raw} to staging files"));
    Ok(())
}

/// Set the modification time on a single file.
pub fn set_file_mtime(path: &Path, mtime: SystemTime) -> Result<()> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("open {} for mtime update", path.display()))?;
    file.set_times(
        std::fs::FileTimes::new()
            .set_accessed(mtime)
            .set_modified(mtime),
    )
    .with_context(|| format!("set mtime on {}", path.display()))?;
    Ok(())
}

/// Set the modification time on a single file from a Unix epoch (seconds).
///
/// Thin wrapper over `set_file_mtime` that accepts `SOURCE_DATE_EPOCH`-style
/// `i64` seconds (signed to permit pre-1970 values per the spec).
pub fn set_file_mtime_epoch(path: &Path, epoch_secs: i64) -> Result<()> {
    let mtime = if epoch_secs >= 0 {
        SystemTime::UNIX_EPOCH + Duration::from_secs(epoch_secs as u64)
    } else {
        SystemTime::UNIX_EPOCH - Duration::from_secs((-epoch_secs) as u64)
    };
    set_file_mtime(path, mtime)
}

/// Recursively pin every regular file's mtime under `dir` to `epoch_secs`
/// (SOURCE_DATE_EPOCH seconds). Packaging tools (makeself's tar, NSIS's `File`)
/// embed each input file's on-disk mtime; `fs::copy` stamps the wall clock, so
/// two harness runs with identical contents drift the packed bytes. Pinning to
/// the build epoch removes that variance.
///
/// Subdirectories are walked; only regular files have their mtime set (mirrors
/// the mtime semantics relevant to the archive headers these tools emit).
pub fn pin_dir_mtimes_epoch(dir: &Path, epoch_secs: i64) -> Result<()> {
    let mut stack: Vec<std::path::PathBuf> = vec![dir.to_path_buf()];
    while let Some(p) = stack.pop() {
        for entry in
            fs::read_dir(&p).with_context(|| format!("read_dir {} for mtime pin", p.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let ft = entry.file_type()?;
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                set_file_mtime_epoch(&path, epoch_secs)
                    .with_context(|| format!("pin mtime on {}", path.display()))?;
            }
        }
    }
    Ok(())
}

/// Recursively copy the directory tree rooted at `src` into `dst`, recreating
/// subdirectories, copying regular files (with [`fs::copy`], which preserves
/// the Unix mode bits — including the executable bit), and recreating symlinks
/// as symlinks rather than dereferencing them.
///
/// Preserving symlinks matters for macOS app bundles, which embed framework
/// version symlinks (`Versions/Current -> A`); a dereferencing copy would
/// flatten them and bloat the bundle. `dst` (and any missing parents) is
/// created if absent. On non-Unix hosts, where creating a symlink needs
/// elevated rights, the link target's contents are copied instead so the tree
/// stays complete.
pub fn copy_dir_tree(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst).with_context(|| format!("create dir {}", dst.display()))?;
    for entry in fs::read_dir(src).with_context(|| format!("read dir {}", src.display()))? {
        let entry = entry.with_context(|| format!("read entry under {}", src.display()))?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        // symlink_metadata (via DirEntry::file_type) so a symlink is recreated
        // as a link rather than dereferenced.
        let file_type = entry
            .file_type()
            .with_context(|| format!("stat {}", from.display()))?;
        if file_type.is_symlink() {
            #[cfg(unix)]
            {
                let target = fs::read_link(&from)
                    .with_context(|| format!("read symlink {}", from.display()))?;
                std::os::unix::fs::symlink(&target, &to).with_context(|| {
                    format!("recreate symlink {} -> {}", to.display(), target.display())
                })?;
            }
            #[cfg(not(unix))]
            {
                if from.is_dir() {
                    copy_dir_tree(&from, &to)?;
                } else {
                    fs::copy(&from, &to)
                        .with_context(|| format!("copy {} to {}", from.display(), to.display()))?;
                }
            }
        } else if file_type.is_dir() {
            copy_dir_tree(&from, &to)?;
        } else {
            fs::copy(&from, &to)
                .with_context(|| format!("copy {} to {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// collect_replace_archives
// ---------------------------------------------------------------------------

/// Collect archive artifact paths for a given crate + target, for removal by `replace` options.
pub fn collect_replace_archives(
    artifacts: &crate::artifact::ArtifactRegistry,
    crate_name: &str,
    target: Option<&str>,
) -> Vec<std::path::PathBuf> {
    artifacts
        .by_kind_and_crate(crate::artifact::ArtifactKind::Archive, crate_name)
        .iter()
        .filter(|a| a.target.as_deref() == target)
        .map(|a| a.path.clone())
        .collect()
}

/// Gated variant of [`collect_replace_archives`]: returns the matching
/// archive paths only when `replace` is `Some(true)`. Used by packaging
/// stages (dmg, msi, flatpak, snapcraft, nsis, pkg, appbundle) to
/// replace a source archive with the packaged output when the user
/// opts in via `replace: true` on the config. Returns an empty vec
/// when `replace` is unset or `false`.
pub fn collect_if_replace(
    replace: Option<bool>,
    artifacts: &crate::artifact::ArtifactRegistry,
    crate_name: &str,
    target: Option<&str>,
) -> Vec<std::path::PathBuf> {
    if replace.unwrap_or(false) {
        collect_replace_archives(artifacts, crate_name, target)
    } else {
        Vec::new()
    }
}

/// Convert any Windows-style backslash separators in `s` to forward
/// slashes. Cross-platform path string normalization for cases where the
/// downstream consumer (artifact-manifest JSON, MSYS subprocess env var)
/// is sensitive to separator drift between Linux/macOS and Windows hosts.
pub fn normalize_path_separators(s: &str) -> String {
    s.replace('\\', "/")
}

/// Apply a "minimal trusted" environment to a `Command` after `env_clear()`.
///
/// Stage subprocess invocations (sbom, source-archive, …) clear the env to
/// stop accidental token leakage but still need a small set of platform-
/// neutral keys so that `git`, `tar`, `syft`, etc. behave normally — HOME
/// for tool config, USER for git author fallback, USERPROFILE/LOCALAPPDATA
/// for the Windows equivalents, TMPDIR/TMP/TEMP so temp-file allocation
/// doesn't land in a forbidden directory, and PATH so the tool itself can
/// find its dependencies. Keeping this list in core means any new entry
/// (e.g. SSL_CERT_DIR for syft pulling enrich data) is added once.
pub fn apply_minimal_env(command: &mut std::process::Command) {
    const PASSTHROUGH: &[&str] = &[
        "HOME",
        "USER",
        "USERPROFILE",
        "TMPDIR",
        "TMP",
        "TEMP",
        "PATH",
        "LOCALAPPDATA",
    ];
    for key in PASSTHROUGH {
        if let Ok(val) = std::env::var(key) {
            command.env(key, val);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // topological_sort tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_topo_sort_simple_chain() {
        let items = vec![
            ("c".to_string(), vec!["b".to_string()]),
            ("b".to_string(), vec!["a".to_string()]),
            ("a".to_string(), vec![]),
        ];
        let sorted = topological_sort(&items);
        assert_eq!(sorted, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_topo_sort_no_deps() {
        let items = vec![("b".to_string(), vec![]), ("a".to_string(), vec![])];
        // Deterministic: alphabetical
        let sorted = topological_sort(&items);
        assert_eq!(sorted, vec!["a", "b"]);
    }

    #[test]
    fn test_topo_sort_ignores_external_deps() {
        let items = vec![
            (
                "b".to_string(),
                vec!["a".to_string(), "external".to_string()],
            ),
            ("a".to_string(), vec![]),
        ];
        let sorted = topological_sort(&items);
        assert_eq!(sorted, vec!["a", "b"]);
    }

    #[test]
    fn test_topo_sort_diamond() {
        let items = vec![
            ("d".to_string(), vec!["b".to_string(), "c".to_string()]),
            ("b".to_string(), vec!["a".to_string()]),
            ("c".to_string(), vec!["a".to_string()]),
            ("a".to_string(), vec![]),
        ];
        let sorted = topological_sort(&items);
        // a must come first, d must come last, b and c in between
        assert_eq!(sorted[0], "a");
        assert_eq!(sorted[3], "d");
    }

    #[test]
    fn test_topo_sort_cycle_appends_remaining() {
        let items = vec![
            ("a".to_string(), vec!["b".to_string()]),
            ("b".to_string(), vec!["a".to_string()]),
            ("c".to_string(), vec![]),
        ];
        let sorted = topological_sort(&items);
        assert_eq!(sorted.len(), 3);
        // c has no deps, should come first; a and b are in a cycle
        assert_eq!(sorted[0], "c");
    }

    #[test]
    fn test_topo_sort_empty() {
        let items: Vec<(String, Vec<String>)> = vec![];
        let sorted = topological_sort(&items);
        assert!(sorted.is_empty());
    }

    // -----------------------------------------------------------------------
    // find_binary tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_find_binary_absolute_path_exists() {
        if cfg!(windows) {
            // cmd.exe exists on all Windows systems
            assert!(find_binary("C:\\Windows\\System32\\cmd.exe"));
        } else {
            // /usr/bin/env exists on virtually all Unix systems
            assert!(find_binary("/usr/bin/env"));
        }
    }

    #[test]
    fn test_find_binary_absolute_path_does_not_exist() {
        if cfg!(windows) {
            assert!(!find_binary("C:\\nonexistent\\binary\\path.exe"));
        } else {
            assert!(!find_binary("/nonexistent/binary/path"));
        }
    }

    #[test]
    fn test_find_binary_bare_name_on_path() {
        if cfg!(windows) {
            // "cmd.exe" should be findable on PATH on any Windows system
            // (find_binary does exact name match, no implicit .exe appending)
            assert!(find_binary("cmd.exe"));
        } else {
            // "env" should be findable on PATH on any Unix system
            assert!(find_binary("env"));
        }
    }

    #[test]
    fn test_find_binary_bare_name_not_on_path() {
        assert!(!find_binary("nonexistent-binary-xyz-12345"));
    }

    // -----------------------------------------------------------------------
    // parse_mod_timestamp tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_mod_timestamp_epoch_integer() {
        let t = parse_mod_timestamp("1704067200").unwrap();
        let epoch = t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(epoch, 1704067200);
    }

    #[test]
    fn test_parse_mod_timestamp_rfc3339() {
        let t = parse_mod_timestamp("2024-01-01T00:00:00Z").unwrap();
        let epoch = t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(epoch, 1704067200);
    }

    #[test]
    fn test_parse_mod_timestamp_rfc3339_with_offset() {
        let t = parse_mod_timestamp("2024-01-01T01:00:00+01:00").unwrap();
        let epoch = t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        // 2024-01-01T01:00:00+01:00 is the same instant as 2024-01-01T00:00:00Z
        assert_eq!(epoch, 1704067200);
    }

    #[test]
    fn test_parse_mod_timestamp_naive_datetime() {
        let t = parse_mod_timestamp("2024-01-01T00:00:00").unwrap();
        let epoch = t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(epoch, 1704067200);
    }

    #[test]
    fn test_parse_mod_timestamp_invalid() {
        let err = parse_mod_timestamp("not-a-timestamp").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not a valid timestamp"),
            "unexpected error: {msg}"
        );
        // The parse error must include
        // the offending mtime value so misconfigurations are diagnosable.
        assert!(
            msg.contains("not-a-timestamp"),
            "error must include the bad value, got: {msg}"
        );
    }

    #[test]
    fn test_parse_mod_timestamp_zero() {
        let t = parse_mod_timestamp("0").unwrap();
        assert_eq!(t, SystemTime::UNIX_EPOCH);
    }

    // -----------------------------------------------------------------------
    // set_file_mtime tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_set_file_mtime_sets_both_atime_and_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();

        let file_path = dir.join("test.txt");
        std::fs::write(&file_path, "hello").unwrap();

        // Set mtime to a known epoch: 2024-01-01T00:00:00Z = 1704067200
        let target = SystemTime::UNIX_EPOCH + Duration::from_secs(1704067200);
        set_file_mtime(&file_path, target).unwrap();

        let meta = std::fs::metadata(&file_path).unwrap();
        let actual_mtime = meta.modified().unwrap();

        // Allow 1-second tolerance for filesystem granularity
        let diff = if actual_mtime > target {
            actual_mtime.duration_since(target).unwrap()
        } else {
            target.duration_since(actual_mtime).unwrap()
        };
        assert!(
            diff.as_secs() <= 1,
            "mtime should be within 1s of target, diff={:?}",
            diff
        );

        // Also verify atime was set (on Linux, accessed() is available)
        let actual_atime = meta.accessed().unwrap();
        let diff_a = if actual_atime > target {
            actual_atime.duration_since(target).unwrap()
        } else {
            target.duration_since(actual_atime).unwrap()
        };
        assert!(
            diff_a.as_secs() <= 1,
            "atime should be within 1s of target, diff={:?}",
            diff_a
        );
    }

    #[test]
    fn test_pin_dir_mtimes_epoch_recurses_into_subdirs() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let sub = dir.join("nested");
        std::fs::create_dir_all(&sub).unwrap();

        let top = dir.join("top.txt");
        let nested = sub.join("nested.txt");
        std::fs::write(&top, "top").unwrap();
        std::fs::write(&nested, "nested").unwrap();

        let epoch: i64 = 1704067200;
        pin_dir_mtimes_epoch(dir, epoch).unwrap();

        let target = SystemTime::UNIX_EPOCH + Duration::from_secs(epoch as u64);
        for path in [&top, &nested] {
            let mtime = std::fs::metadata(path).unwrap().modified().unwrap();
            assert_eq!(
                mtime,
                target,
                "{}: mtime must equal the pinned epoch exactly",
                path.display()
            );
        }
    }

    #[test]
    fn test_set_file_mtime_nonexistent_file() {
        let result = set_file_mtime(Path::new("/nonexistent/file.txt"), SystemTime::UNIX_EPOCH);
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // apply_mod_timestamp tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_apply_mod_timestamp_sets_mtime_on_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();

        // Create two regular files and a subdirectory (the dir itself is not stamped)
        std::fs::write(dir.join("a.txt"), "aaa").unwrap();
        std::fs::write(dir.join("b.txt"), "bbb").unwrap();
        std::fs::create_dir(dir.join("subdir")).unwrap();

        let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Quiet);
        apply_mod_timestamp(dir, "1704067200", &log).unwrap();

        let target = SystemTime::UNIX_EPOCH + Duration::from_secs(1704067200);
        for name in &["a.txt", "b.txt"] {
            let meta = std::fs::metadata(dir.join(name)).unwrap();
            let mtime = meta.modified().unwrap();
            let diff = if mtime > target {
                mtime.duration_since(target).unwrap()
            } else {
                target.duration_since(mtime).unwrap()
            };
            assert!(
                diff.as_secs() <= 1,
                "{name}: mtime should be within 1s of target, diff={:?}",
                diff
            );
        }
    }

    #[test]
    fn test_apply_mod_timestamp_recurses_into_subdirs() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let sub = dir.join("docs");
        std::fs::create_dir_all(&sub).unwrap();

        let top = dir.join("top.txt");
        let nested = sub.join("README.txt");
        std::fs::write(&top, "top").unwrap();
        std::fs::write(&nested, "nested").unwrap();

        let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Quiet);
        apply_mod_timestamp(dir, "1704067200", &log).unwrap();

        let target = SystemTime::UNIX_EPOCH + Duration::from_secs(1704067200);
        for path in [&top, &nested] {
            let mtime = std::fs::metadata(path).unwrap().modified().unwrap();
            let diff = if mtime > target {
                mtime.duration_since(target).unwrap()
            } else {
                target.duration_since(mtime).unwrap()
            };
            assert!(
                diff.as_secs() <= 1,
                "{}: nested file must receive mod_timestamp, diff={:?}",
                path.display(),
                diff
            );
        }
    }

    #[test]
    fn test_apply_mod_timestamp_invalid_timestamp_errors() {
        let dir = tempfile::tempdir().unwrap();

        let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Quiet);
        let result = apply_mod_timestamp(dir.path(), "not-valid", &log);
        assert!(result.is_err());
    }
}
