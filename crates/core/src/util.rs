use std::path::Path;

/// Check whether a binary can be found on the system.
///
/// For absolute or relative paths (containing `/`), checks if the file exists.
/// For bare names, searches each directory in the `PATH` environment variable
/// for an executable with the given name. This is a pure-Rust implementation
/// that avoids shelling out to `which` or `command -v`, making it portable
/// across all platforms.
pub fn find_binary(name: &str) -> bool {
    if name.contains('/') {
        return Path::new(name).exists();
    }

    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return true;
            }
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_binary_absolute_path_exists() {
        // /usr/bin/env exists on virtually all Unix systems
        assert!(find_binary("/usr/bin/env"));
    }

    #[test]
    fn test_find_binary_absolute_path_does_not_exist() {
        assert!(!find_binary("/nonexistent/binary/path"));
    }

    #[test]
    fn test_find_binary_bare_name_on_path() {
        // "env" should be findable on PATH on any Unix system
        assert!(find_binary("env"));
    }

    #[test]
    fn test_find_binary_bare_name_not_on_path() {
        assert!(!find_binary("nonexistent-binary-xyz-12345"));
    }
}
