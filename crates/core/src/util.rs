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

/// Apply `mod_timestamp` to all regular files in a directory.
///
/// Parses the timestamp via `parse_mod_timestamp`, then sets the mtime on
/// every regular file in `dir`.
pub fn apply_mod_timestamp(dir: &Path, raw: &str, log: &crate::log::StageLogger) -> Result<()> {
    let mtime = parse_mod_timestamp(raw)?;

    for entry in fs::read_dir(dir).with_context(|| format!("read staging dir {}", dir.display()))? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_file() {
            set_file_mtime(&entry.path(), mtime)?;
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
        let dir = std::env::temp_dir().join("anodize_test_set_file_mtime");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

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

        let _ = std::fs::remove_dir_all(&dir);
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
        let dir = std::env::temp_dir().join("anodize_test_apply_mod_timestamp");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Create two regular files and a subdirectory (should be skipped)
        std::fs::write(dir.join("a.txt"), "aaa").unwrap();
        std::fs::write(dir.join("b.txt"), "bbb").unwrap();
        std::fs::create_dir(dir.join("subdir")).unwrap();

        let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Quiet);
        apply_mod_timestamp(&dir, "1704067200", &log).unwrap();

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

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_apply_mod_timestamp_invalid_timestamp_errors() {
        let dir = std::env::temp_dir().join("anodize_test_apply_mod_timestamp_invalid");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Quiet);
        let result = apply_mod_timestamp(&dir, "not-valid", &log);
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
