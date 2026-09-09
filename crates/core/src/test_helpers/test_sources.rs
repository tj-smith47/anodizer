//! What counts as a whole test source file, and the premise that makes
//! skipping one by name sound.
//!
//! The `.claude/scripts/**` scanners classify a file as test code by NAME
//! (`lib/test-regions.awk`'s `is_test_file`) because a sibling `tests.rs`,
//! a `<name>_tests.rs` and a `crates/<crate>/tests/**` integration file carry
//! no `#[cfg(test)]` of their own. Rust-side structural walks need the same
//! two answers, and a second spelling of either one drifts silently: a walk
//! that stops agreeing with the lexer reports on code the scanners skip, or
//! skips code they report on.
//!
//! - [`is_test_source_path`] mirrors `is_test_file` rule for rule.
//! - [`declared_under_test_cfg`] checks the premise behind the name match —
//!   the parent module really does declare the file under a test-only `cfg`.

use std::path::{Path, PathBuf};

/// Whether `path` is a whole test source file by name, exactly as
/// `.claude/scripts/lib/test-regions.awk`'s `is_test_file` decides it.
///
/// Three rules, matching the awk lexer's two alternatives:
///
/// - the file is `tests.rs`;
/// - the file is `<name>_tests.rs`, where `<name>` is `[a-z0-9_]*`;
/// - the path runs through a `crates/<crate>/tests/` directory (an
///   integration-test target), with at least one component below it.
///
/// ```
/// # use anodizer_core::test_helpers::test_sources::is_test_source_path;
/// # use std::path::Path;
/// assert!(is_test_source_path(Path::new("crates/core/src/tests.rs")));
/// assert!(is_test_source_path(Path::new("crates/core/src/log_tests.rs")));
/// assert!(is_test_source_path(Path::new("crates/core/tests/cli.rs")));
/// assert!(!is_test_source_path(Path::new("crates/core/src/mytests.rs")));
/// ```
pub fn is_test_source_path(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    if let Some(prefix) = name.strip_suffix("tests.rs")
        && (prefix.is_empty()
            || (prefix.ends_with('_')
                && prefix
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')))
    {
        return true;
    }
    let parts: Vec<&str> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    parts
        .windows(3)
        .enumerate()
        .any(|(i, w)| w[0] == "crates" && w[2] == "tests" && i + 3 < parts.len())
}

/// Verify that `module_file`'s parent module declares it as `mod <stem>;`
/// under a test-only `cfg` — the premise that makes classifying a file as
/// test code by name equivalent to classifying it by compilation.
///
/// `module_file` is a `.rs` file or a module directory. The parent is
/// `mod.rs`/`lib.rs`/`main.rs` beside it, or the 2018-layout `<dir>.rs` next
/// to its directory. The gating attribute may sit on the item's own line or
/// anywhere in the contiguous run of attribute and comment lines directly
/// above it, and may be `#[cfg(test)]`, `#[cfg(all(test, …))]` or
/// `#[cfg(any(…, test, …))]`; `test` must appear as a whole token and a
/// `not(…)` predicate never gates.
///
/// The `Err` message names both the module file and the parent that should
/// have declared it.
pub fn declared_under_test_cfg(module_file: &Path) -> Result<(), String> {
    let stem = module_file
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("{} has no module name", module_file.display()))?;
    let dir = module_file
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", module_file.display()))?;
    let sibling: Option<PathBuf> = dir
        .file_name()
        .map(|name| dir.with_file_name(format!("{}.rs", name.to_string_lossy())));
    let parent = ["mod.rs", "lib.rs", "main.rs"]
        .iter()
        .map(|name| dir.join(name))
        .chain(sibling)
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| format!("no parent module file for {}", module_file.display()))?;
    let text =
        std::fs::read_to_string(&parent).map_err(|e| format!("read {}: {e}", parent.display()))?;
    let lines: Vec<&str> = text.lines().collect();
    let item = lines
        .iter()
        .position(|line| is_mod_item(line, stem))
        .ok_or_else(|| {
            format!(
                "{} declares no `mod {stem};` for {}",
                parent.display(),
                module_file.display()
            )
        })?;
    let gated = (0..=item)
        .rev()
        .take_while(|&i| {
            let trimmed = lines[i].trim_start();
            i == item || trimmed.starts_with("#[") || trimmed.starts_with("//")
        })
        .any(|i| gates_on_test(lines[i]));
    if gated {
        return Ok(());
    }
    Err(format!(
        "{} must be declared under a test-only `cfg` in {}",
        module_file.display(),
        parent.display()
    ))
}

/// Whether a source line's code — trailing `//` comment stripped, any
/// same-line attributes stripped — is exactly the `mod <stem>;` item
/// (`pub`, `pub(crate)` and the like allowed). A comment never matches.
fn is_mod_item(line: &str, stem: &str) -> bool {
    let mut code = line.split("//").next().unwrap_or("").trim();
    while let Some(rest) = code.strip_prefix("#[") {
        let Some(close) = rest.find(']') else {
            return false;
        };
        code = rest[close + 1..].trim_start();
    }
    if let Some(rest) = code.strip_prefix("pub") {
        let rest = match rest.strip_prefix('(') {
            Some(vis) => match vis.find(')') {
                Some(close) => &vis[close + 1..],
                None => return false,
            },
            None => rest,
        };
        if !rest.starts_with(char::is_whitespace) {
            return false;
        }
        code = rest.trim_start();
    }
    code.strip_prefix("mod")
        .filter(|rest| rest.starts_with(char::is_whitespace))
        .map(|rest| rest.trim_start())
        .and_then(|rest| rest.strip_prefix(stem))
        .is_some_and(|rest| rest.trim_start() == ";")
}

/// Whether a line carries a `cfg(…)` predicate that names `test` as a whole
/// token outside any `not(…)`.
fn gates_on_test(line: &str) -> bool {
    let Some(open) = line.find("cfg(") else {
        return false;
    };
    let rest = &line[open + "cfg(".len()..];
    let mut depth = 1usize;
    let mut end = rest.len();
    for (index, ch) in rest.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end = index;
                    break;
                }
            }
            _ => {}
        }
    }
    let predicate = &rest[..end];
    !predicate.contains("not(")
        && predicate
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .any(|token| token == "test")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lay out `<tmp>/<parent>` with the given parent-module text and a
    /// `tests.rs` (or `tests/` directory) inside `<tmp>/m`; returns the
    /// module path the premise check is asked about.
    fn synthetic_module(
        tmp: &Path,
        parent_file: &str,
        parent_text: &str,
        tests_dir: bool,
    ) -> PathBuf {
        let module = tmp.join("m");
        std::fs::create_dir_all(&module).unwrap();
        std::fs::write(tmp.join(parent_file), parent_text).unwrap();
        if tests_dir {
            let dir = module.join("tests");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("mod.rs"), "").unwrap();
            dir
        } else {
            let file = module.join("tests.rs");
            std::fs::write(&file, "").unwrap();
            file
        }
    }

    /// `#[cfg(test)] mod tests;` on one line is a gated declaration.
    #[test]
    fn cfg_test_on_the_same_line_is_accepted() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tests = synthetic_module(tmp.path(), "m/mod.rs", "#[cfg(test)] mod tests;\n", false);
        declared_under_test_cfg(&tests).unwrap();
    }

    /// A second attribute between `#[cfg(test)]` and the item is still gated.
    #[test]
    fn cfg_test_above_another_attribute_is_accepted() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tests = synthetic_module(
            tmp.path(),
            "m/mod.rs",
            "#[cfg(test)]\n#[allow(clippy::unwrap_used)]\nmod tests;\n",
            false,
        );
        declared_under_test_cfg(&tests).unwrap();
    }

    /// A doc comment between `#[cfg(test)]` and the item is still gated.
    #[test]
    fn cfg_test_above_a_doc_comment_is_accepted() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tests = synthetic_module(
            tmp.path(),
            "m/mod.rs",
            "#[cfg(test)]\n/// Unit tests.\npub(crate) mod tests;\n",
            false,
        );
        declared_under_test_cfg(&tests).unwrap();
    }

    /// An `all(…)` conjunction naming `test` gates the module.
    #[test]
    fn cfg_all_test_and_unix_is_accepted() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tests = synthetic_module(
            tmp.path(),
            "m/mod.rs",
            "#[cfg(all(test, unix))]\nmod tests;\n",
            false,
        );
        declared_under_test_cfg(&tests).unwrap();
    }

    /// An `any(…)` disjunction naming `test` gates the module.
    #[test]
    fn cfg_any_test_or_feature_is_accepted() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tests = synthetic_module(
            tmp.path(),
            "m/mod.rs",
            "#[cfg(any(test, feature = \"x\"))]\nmod tests;\n",
            false,
        );
        declared_under_test_cfg(&tests).unwrap();
    }

    /// `not(test)` compiles the module OUTSIDE tests, so it never gates —
    /// and neither does a token that merely contains `test`.
    #[test]
    fn cfg_not_test_and_a_longer_token_never_gate() {
        assert!(!gates_on_test("#[cfg(not(test))]"));
        assert!(!gates_on_test("#[cfg(feature = \"testing\")]"));
        assert!(gates_on_test("#[cfg(test)]"));
    }

    /// The 2018 layout declares `m/tests.rs` from `m.rs` beside the directory.
    #[test]
    fn sibling_parent_file_is_found() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tests = synthetic_module(tmp.path(), "m.rs", "#[cfg(test)]\nmod tests;\n", false);
        declared_under_test_cfg(&tests).unwrap();
    }

    /// A `tests/` directory needs the same gated declaration.
    #[test]
    fn tests_directory_needs_the_same_declaration() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tests = synthetic_module(tmp.path(), "m/mod.rs", "#[cfg(test)]\nmod tests;\n", true);
        declared_under_test_cfg(&tests).unwrap();

        std::fs::write(tests.parent().unwrap().join("mod.rs"), "mod tests;\n").unwrap();
        declared_under_test_cfg(&tests).expect_err("an ungated tests/ directory is not declared");
    }

    /// A comment ending in `mod tests` is not the item; the real gated item
    /// below it is what the check must find.
    #[test]
    fn comment_ending_in_mod_tests_is_not_the_item() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tests = synthetic_module(
            tmp.path(),
            "m/mod.rs",
            "// helpers shared with mod tests\nfn helper() {}\n#[cfg(test)]\nmod tests;\n",
            false,
        );
        declared_under_test_cfg(&tests).unwrap();
    }

    /// An ungated `mod tests;` fails with a message naming both files.
    #[test]
    fn ungated_mod_tests_fails_naming_both_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tests = synthetic_module(tmp.path(), "m/mod.rs", "mod tests;\n", false);
        let msg = declared_under_test_cfg(&tests).expect_err("an ungated declaration is rejected");
        let parent = tmp.path().join("m").join("mod.rs");
        assert!(
            msg.contains(&tests.display().to_string())
                && msg.contains(&parent.display().to_string()),
            "message must name the module file and its parent: {msg}"
        );
    }

    /// A parent that declares no `mod <stem>;` at all names both files too.
    #[test]
    fn missing_declaration_fails_naming_both_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tests = synthetic_module(tmp.path(), "m/mod.rs", "fn helper() {}\n", false);
        let msg = declared_under_test_cfg(&tests).expect_err("no declaration is rejected");
        assert!(
            msg.contains("mod tests;") && msg.contains(&tests.display().to_string()),
            "message must name the missing item and the module file: {msg}"
        );
    }

    /// The three name rules, and the shapes that look like them but are not.
    #[test]
    fn name_rules_match_the_awk_lexer() {
        for path in [
            "crates/demo/src/tests.rs",
            "crates/demo/src/foo_tests.rs",
            "crates/demo/src/_tests.rs",
            "crates/demo/tests/integration.rs",
            "crates/demo/tests/nested/case.rs",
        ] {
            assert!(is_test_source_path(Path::new(path)), "{path}");
        }
        for path in [
            "crates/demo/src/lib.rs",
            "crates/demo/src/mytests.rs",
            "crates/demo/src/Foo_tests.rs",
            "crates/demo/src/tests.rs.bak",
            "crates/demo/src/process/tests/mod.rs",
            "crates/demo/tests",
            "src/tests/helper.rs",
        ] {
            assert!(!is_test_source_path(Path::new(path)), "{path}");
        }
    }
}
