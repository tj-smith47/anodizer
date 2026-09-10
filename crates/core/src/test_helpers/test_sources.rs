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
//! - [`production_half`] finds where a production file's inline test module
//!   starts, so a walk over production code stops before it.
//! - [`rust_sources`] is the walk itself: every production `.rs` file under a
//!   directory, with test sources skipped by name once their declaration has
//!   been checked. [`test_sources`] is that walk's other half — the sources it
//!   skipped, unchecked, for the caller that checks them. Both come from
//!   `partition_sources`, the one directory walk in the workspace.
//! - [`function_bodies`] splits a source into per-function bodies, the grain a
//!   structural guard needs to ask what one function does.

use std::path::{Path, PathBuf};

/// Every production `.rs` file under `dir`, recursively.
///
/// Test sources are skipped by NAME — whatever [`is_test_source_path`] names,
/// plus a `tests/` module directory — rather than by their content, because a
/// sibling test file carries no `#[cfg(test)]` of its own. Skipping one by
/// name is only sound while it really is test-only, so every skipped path is
/// checked against its parent module's declaration and an ungated one panics
/// rather than silently dropping production code from the walk.
pub fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let (production, tests) = partition_sources(dir);
    for path in &tests {
        declared_under_test_cfg(path).unwrap_or_else(|why| panic!("{why}"));
    }
    production
}

/// The other half of [`rust_sources`]'s walk: every test source it skips — a
/// whole test source file by name, and a `tests/` module directory itself,
/// which is the unit a parent module declares.
///
/// Returned without checking the declarations, for the caller whose job is to
/// check them: the two directions come from one walk, so a rule either learns
/// cannot leave the other behind.
pub fn test_sources(dir: &Path) -> Vec<PathBuf> {
    partition_sources(dir).1
}

/// Split every `.rs` file under `dir` into (production, test sources) by the
/// name rules above, descending into every directory but a `tests/` module
/// directory, which is itself one test source.
fn partition_sources(dir: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut production = Vec::new();
    let mut tests = Vec::new();
    for entry in std::fs::read_dir(dir).expect("read source dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "tests") {
                tests.push(path);
                continue;
            }
            let (nested_production, nested_tests) = partition_sources(&path);
            production.extend(nested_production);
            tests.extend(nested_tests);
        } else if path.extension().is_some_and(|e| e == "rs") {
            if is_test_source_path(&path) {
                tests.push(path);
            } else {
                production.push(path);
            }
        }
    }
    (production, tests)
}

/// Whether a trimmed source line opens a `fn` item: an optional visibility
/// (`pub`, `pub(crate)`, `pub(super)`, `pub(in path)`), then any run of item
/// qualifiers (`default`, `const`, `async`, `unsafe`, `extern "C"`), then
/// `fn `.
///
/// Spelled as a rule rather than as a list of the four commonest prefixes,
/// because a list leaves `async fn`, `const fn`, `unsafe fn` and
/// `pub(in …) fn` — all of which this workspace contains — unexamined by every
/// guard that composes [`function_bodies`], and a guard that cannot see a
/// shape cannot fail on it.
fn opens_fn_item(trimmed: &str) -> bool {
    let mut rest = trimmed;
    if let Some(after_pub) = rest.strip_prefix("pub") {
        rest = match after_pub.strip_prefix('(') {
            Some(scoped) => match scoped.split_once(')') {
                Some((_, tail)) => tail,
                None => return false,
            },
            None => after_pub,
        };
        if !rest.starts_with(' ') {
            return false;
        }
    }
    loop {
        rest = rest.trim_start();
        if rest.starts_with("fn ") {
            return true;
        }
        let Some((word, tail)) = rest.split_once(' ') else {
            return false;
        };
        // A quoted ABI is the second half of `extern "C"`; every other
        // qualifier is a bare keyword.
        if !matches!(word, "default" | "const" | "async" | "unsafe" | "extern")
            && !word.starts_with('"')
        {
            return false;
        }
        rest = tail;
    }
}

/// Split Rust source into function bodies: a `fn` line opens a body that ends
/// at the first line closing a brace at the `fn`'s own indent. Every `fn`
/// spelling [`opens_fn_item`] accepts opens one.
///
/// A structural guard that asks "does any ONE function do both of these
/// things" needs the per-function grain — a whole-file substring search
/// answers a different, weaker question.
pub fn function_bodies(src: &str) -> Vec<String> {
    let lines: Vec<&str> = src.lines().collect();
    let mut bodies = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if !opens_fn_item(trimmed) {
            continue;
        }
        let indent = line.len() - trimmed.len();
        let closing = format!("{}}}", " ".repeat(indent));
        let end = lines[i + 1..]
            .iter()
            .position(|l| *l == closing)
            .map(|p| i + 1 + p)
            .unwrap_or(lines.len() - 1);
        bodies.push(lines[i..=end].join("\n"));
    }
    bodies
}

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
/// above it, and must be test-only by [`is_test_only_cfg`] — `#[cfg(test)]`
/// or an `#[cfg(all(…))]` naming `test`, never an `any(…)` or a `not(test)`.
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
        .any(|i| is_test_only_cfg(lines[i]));
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

/// Whether `line` is an attribute whose `cfg(…)` predicate can hold ONLY
/// under `cargo test`, exactly as `.claude/scripts/lib/rust-lex.awk`'s
/// `is_test_only_cfg` decides it.
///
/// Gates: bare `test`, and an `all(…)` one of whose top-level terms is
/// itself test-only — so a conjunction may carry any other terms it likes,
/// `not(…)` ones included.
///
/// Never gates: an `any(…)` wrapping the `test` term at any depth (the
/// disjunction is satisfied by its other terms, so the item compiles into a
/// production build too), `not(test)`, and any predicate that never names
/// `test` as a term.
///
/// ```
/// # use anodizer_core::test_helpers::test_sources::is_test_only_cfg;
/// assert!(is_test_only_cfg("#[cfg(test)]"));
/// assert!(is_test_only_cfg("#[cfg(all(test, not(windows)))]"));
/// assert!(!is_test_only_cfg("#[cfg(any(test, feature = \"x\"))]"));
/// assert!(!is_test_only_cfg("#[cfg(not(test))]"));
/// ```
pub fn is_test_only_cfg(line: &str) -> bool {
    line.trim_start()
        .strip_prefix("#[cfg(")
        .and_then(split_group)
        .is_some_and(|(predicate, _)| predicate_is_test_only(predicate))
}

/// The production half of one Rust source file: everything before its inline
/// test module. That module starts at the first top-level `#[cfg(…)]` that is
/// test-only ([`is_test_only_cfg`]) and gates an item written out in place
/// (`mod tests {` …). A `mod tests;` declaration does not start one — its body
/// is a file the name rules already classify, and production items follow the
/// declaration — and neither does a `cfg` nested inside another item. A file
/// with no inline test module is returned whole.
///
/// ```
/// # use anodizer_core::test_helpers::test_sources::production_half;
/// let inline = "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn b() {}\n}\n";
/// assert_eq!(production_half(inline), "fn a() {}\n");
/// let sibling = "#[cfg(test)]\nmod tests;\nfn a() {}\n";
/// assert_eq!(production_half(sibling), sibling);
/// ```
pub fn production_half(text: &str) -> &str {
    let mut offset = 0usize;
    let mut cfg_at: Option<usize> = None;
    for line in text.split_inclusive('\n') {
        if line.starts_with("#[cfg(") && is_test_only_cfg(line) {
            cfg_at.get_or_insert(offset);
        } else if let Some(start) = cfg_at
            && !line.starts_with("#[")
            && !line.starts_with("//")
            && !line.trim().is_empty()
        {
            // The gated item itself: a body opened in place is the inline test
            // module; anything else (a declaration, a `use`) is not.
            if line.trim_end().ends_with('{') {
                return &text[..start];
            }
            cfg_at = None;
        }
        offset += line.len();
    }
    text
}

/// Split `rest` — the text after an opening `(` — at the `)` that closes it,
/// into the group's contents and whatever follows the `)`. `None` when the
/// group never closes.
fn split_group(rest: &str) -> Option<(&str, &str)> {
    let mut depth = 1usize;
    for (index, ch) in rest.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((&rest[..index], &rest[index + 1..]));
                }
            }
            _ => {}
        }
    }
    None
}

/// Whether one `cfg` predicate term is test-only; see [`is_test_only_cfg`].
fn predicate_is_test_only(predicate: &str) -> bool {
    let predicate = predicate.trim();
    if predicate == "test" {
        return true;
    }
    let Some((terms, tail)) = predicate.strip_prefix("all(").and_then(split_group) else {
        return false;
    };
    tail.is_empty() && top_level_terms(terms).any(predicate_is_test_only)
}

/// The comma-separated terms of a predicate list, split only at nesting
/// depth zero so a term's own parenthesised list survives intact.
fn top_level_terms(terms: &str) -> impl Iterator<Item = &str> {
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut out = Vec::new();
    for (index, ch) in terms.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                out.push(&terms[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    out.push(&terms[start..]);
    out.into_iter()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `.rs` file the workspace ships, production and test alike,
    /// reached through the shared walk itself. An integration-test directory
    /// is only expanded: its files are declared by cargo's target layout, not
    /// by a parent module, so the declaration check does not apply to them.
    fn workspace_sources() -> Vec<std::path::PathBuf> {
        fn expand(dir: &Path, out: &mut Vec<PathBuf>) {
            for path in test_sources(dir) {
                if path.is_dir() {
                    expand(&path, out);
                } else {
                    out.push(path);
                }
            }
        }
        let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&crates).expect("crates dir") {
            let krate = entry.expect("crate entry").path();
            let src = krate.join("src");
            if src.is_dir() {
                out.extend(rust_sources(&src));
                expand(&src, &mut out);
            }
            let tests = krate.join("tests");
            if tests.is_dir() {
                // An integration-test file is named for what it tests
                // (`integration.rs`), not `tests.rs`, so the name rules put it
                // in the production half of its own directory. Both halves are
                // sources a structural guard must see. Split directly rather
                // than asked for through `rust_sources`, whose parent-module
                // check has no answer here: an integration test root is
                // declared by cargo, not by a `mod` item.
                out.extend(partition_sources(&tests).0);
                expand(&tests, &mut out);
            }
        }
        assert!(!out.is_empty(), "no source under {}", crates.display());
        out
    }

    /// [`partition_sources`] is the only directory walk that picks Rust files
    /// out of a tree. A second one drifts from it silently — a rule taught to
    /// one walk and not the other reports on code the scanners skip, or skips
    /// code they report on — and two such copies had already drifted before
    /// this guard existed. Any function that both reads a directory and tests
    /// for the Rust file extension fails here until it goes through this walk.
    /// Whether a function body is a Rust-source directory walk: it reads a
    /// directory AND tests for the extension. Both spellings count — an
    /// `extension() == "rs"` compare and a `ends_with(".rs")` one — because a
    /// detector that knows only the first is blind to the second, which
    /// contains no `"rs"` token at all.
    fn is_rust_source_walk(body: &str) -> bool {
        body.contains("read_dir(") && (body.contains("\"rs\"") || body.contains("\".rs\""))
    }

    /// The renderer internals a documented-render pin needs are re-exported
    /// only under the `test-helpers` feature. Ungated they are permanent
    /// public API of a consumer-facing library, and un-promising them later is
    /// a breaking change — the cheaper direction is not to promise them.
    #[test]
    fn the_renderer_internals_are_re_exported_only_under_the_test_feature() {
        let mod_rs = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/log/mod.rs");
        let text = std::fs::read_to_string(&mod_rs).expect("readable log module");
        // Read the whole `pub use` STATEMENT, not one line of it: rustfmt
        // wraps a long re-export list across lines, and a line-anchored check
        // then sees a `pub use render::{` carrying none of the names.
        for (idx, _) in text.match_indices("pub use render::{") {
            let end = text[idx..].find("};").expect("a terminated re-export") + idx;
            let statement = &text[idx..end];
            // Only the attribute/doc run directly above the statement gates
            // it. Stopping at the first blank line instead would find a
            // neighbouring re-export's `cfg` and call every statement gated.
            let gated = text[..idx]
                .lines()
                .rev()
                .take_while(|l| {
                    let t = l.trim_start();
                    t.starts_with("#[") || t.starts_with("//")
                })
                .any(|l| l.contains("cfg(feature = \"test-helpers\")"));
            for name in ["render_kv_row", "render_stage_header_line", "strip_ansi"] {
                assert!(
                    !statement.contains(name) || gated,
                    "`{name}` is re-exported without the test-helpers gate:\n{statement}"
                );
            }
        }
    }

    /// The ANSI stripper is defined once, in the log renderer. Three
    /// definitions existed — two of them in the CLI's integration tests — and
    /// each answered a different question about what an escape sequence is, so
    /// a test could pass against a line the renderer never produced.
    ///
    /// The needle is assembled at runtime so this test's own source does not
    /// read as a definition.
    #[test]
    fn the_ansi_stripper_is_defined_once_in_the_workspace() {
        let needle = format!("fn {}(", "strip_ansi");
        let mut defs = Vec::new();
        for source in workspace_sources() {
            let text = std::fs::read_to_string(&source).expect("readable source");
            for _ in 0..text.matches(&needle).count() {
                defs.push(source.display().to_string());
            }
        }
        assert_eq!(
            defs.len(),
            1,
            "the stripper belongs to the renderer that writes the escapes: {defs:?}"
        );
        assert!(
            defs[0].contains("core/src/log/render.rs"),
            "the one definition is the renderer's: {defs:?}"
        );
    }

    /// The walk detector must see both ways a source names the extension. A
    /// `.ends_with(".rs")` walker contains no `"rs"` token, so a detector
    /// spelled only the first way welcomes it with a green suite.
    #[test]
    fn the_walk_detector_reads_both_extension_spellings() {
        assert!(is_rust_source_walk(
            "fn a(d: &Path) { for e in std::fs::read_dir(d)? { if e.path().extension() == Some(\"rs\".as_ref()) {} } }"
        ));
        assert!(is_rust_source_walk(
            "fn b(d: &Path) { for e in std::fs::read_dir(d)? { if e.path().to_string_lossy().ends_with(\".rs\") {} } }"
        ));
        // A directory read that never asks about the extension is not a
        // Rust-source walk.
        assert!(!is_rust_source_walk(
            "fn c(d: &Path) { std::fs::read_dir(d) }"
        ));
    }

    /// Every `fn` spelling this workspace contains must open a body, or a
    /// structural guard composing the split simply never examines the
    /// function — the shape it was built to catch passes unseen.
    #[test]
    fn function_bodies_opens_every_fn_spelling() {
        let src = "\
fn plain() {
}
pub fn public() {
}
pub(crate) fn crate_scoped() {
}
pub(super) fn super_scoped() {
}
pub(in crate::a) fn path_scoped() {
}
async fn asynchronous() {
}
const fn constant() {
}
unsafe fn unsafely() {
}
pub async unsafe fn qualified() {
}
unsafe extern \"C\" fn abi() {
}
const NOT_A_FN: usize = 1;
unsafe impl Send for NotAFn {
}
";
        let names: Vec<String> = function_bodies(src)
            .iter()
            .map(|b| {
                b.split_once("fn ")
                    .and_then(|(_, rest)| rest.split('(').next())
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        assert_eq!(
            names,
            vec![
                "plain",
                "public",
                "crate_scoped",
                "super_scoped",
                "path_scoped",
                "asynchronous",
                "constant",
                "unsafely",
                "qualified",
                "abi",
            ],
        );
    }

    #[test]
    fn every_rust_source_walk_comes_from_the_shared_scanner() {
        let mut walks = Vec::new();
        for source in workspace_sources() {
            let text = std::fs::read_to_string(&source).expect("readable source");
            for body in function_bodies(&text) {
                if !is_rust_source_walk(&body) {
                    continue;
                }
                let name = body
                    .split_once("fn ")
                    .and_then(|(_, rest)| rest.split(['(', '<']).next())
                    .unwrap_or_default()
                    .to_string();
                walks.push(format!("{}: {name}", source.display()));
            }
        }
        // `crate_has_binary_target` lists one crate's `src/bin` to answer
        // whether a binary target exists at all; it collects no source to
        // scan, so it is not a walk this module can serve.
        const ALLOWED: [&str; 2] = [
            "core/src/test_helpers/test_sources.rs: partition_sources",
            "stage-build/src/command.rs: crate_has_binary_target",
        ];
        let mut found: Vec<String> = walks
            .iter()
            .map(|w| {
                ALLOWED
                    .iter()
                    .find(|a| w.ends_with(*a))
                    .map_or_else(|| w.clone(), |a| (*a).to_string())
            })
            .collect();
        found.sort();
        let mut expected: Vec<String> = ALLOWED.iter().map(|a| (*a).to_string()).collect();
        expected.sort();
        assert_eq!(
            found, expected,
            "a Rust-source walk outside the shared scanner: it must take its \
             files from `rust_sources` / `test_sources`"
        );
    }

    /// The walk skips a `tests/` module directory, and only while its parent
    /// really declares it under a test-only `cfg`: gated, the walk yields
    /// only the parent module; ungated, the walk fails rather than silently
    /// dropping the production code it would have scanned.
    #[test]
    fn walk_skips_only_a_gated_tests_directory() {
        let tmp = tempfile::TempDir::new().unwrap();
        let module = tmp.path().join("m");
        std::fs::create_dir_all(module.join("tests")).unwrap();
        std::fs::write(module.join("tests").join("mod.rs"), "").unwrap();
        std::fs::write(module.join("mod.rs"), "#[cfg(test)]\nmod tests;\n").unwrap();
        assert_eq!(rust_sources(&module), vec![module.join("mod.rs")]);

        std::fs::write(module.join("mod.rs"), "mod tests;\n").unwrap();
        assert!(
            std::panic::catch_unwind(|| rust_sources(&module)).is_err(),
            "an ungated tests/ directory must fail the walk"
        );
    }

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

    /// An `any(…)` disjunction compiles the module into a production build
    /// whenever another term holds, so it never gates — and the failure
    /// names both files, like every other ungated declaration.
    #[test]
    fn cfg_any_test_or_feature_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tests = synthetic_module(
            tmp.path(),
            "m/mod.rs",
            "#[cfg(any(test, feature = \"x\"))]\nmod tests;\n",
            false,
        );
        let msg = declared_under_test_cfg(&tests).expect_err("`any(…)` is not a test-only gate");
        let parent = tmp.path().join("m").join("mod.rs");
        assert!(
            msg.contains(&tests.display().to_string())
                && msg.contains(&parent.display().to_string()),
            "message must name the module file and its parent: {msg}"
        );
    }

    /// A `not(…)` term BESIDE a top-level `test` is a legitimate gate: the
    /// conjunction still cannot hold outside `cargo test`.
    #[test]
    fn cfg_all_test_and_not_windows_is_accepted() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tests = synthetic_module(
            tmp.path(),
            "m/mod.rs",
            "#[cfg(all(test, not(windows)))]\nmod tests;\n",
            false,
        );
        declared_under_test_cfg(&tests).unwrap();
    }

    /// The `test` term of an `all(…)` may sit anywhere in the list.
    #[test]
    fn cfg_all_feature_then_test_is_accepted() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tests = synthetic_module(
            tmp.path(),
            "m/mod.rs",
            "#[cfg(all(feature = \"x\", test))]\nmod tests;\n",
            false,
        );
        declared_under_test_cfg(&tests).unwrap();
    }

    /// `not(test)` compiles the module OUTSIDE tests, so it never gates.
    #[test]
    fn cfg_not_test_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tests = synthetic_module(
            tmp.path(),
            "m/mod.rs",
            "#[cfg(not(test))]\nmod tests;\n",
            false,
        );
        let msg = declared_under_test_cfg(&tests).expect_err("`not(test)` is not a test-only gate");
        let parent = tmp.path().join("m").join("mod.rs");
        assert!(
            msg.contains(&tests.display().to_string())
                && msg.contains(&parent.display().to_string()),
            "message must name the module file and its parent: {msg}"
        );
    }

    /// The predicate shapes the parser rules on, straight to the predicate.
    #[test]
    fn only_a_test_only_predicate_gates() {
        for line in [
            "#[cfg(test)]",
            "  #[cfg(test)] mod tests;",
            "#[cfg(all(test, unix))]",
            "#[cfg(all(test, not(windows)))]",
            "#[cfg(all(feature = \"x\", test))]",
            "#[cfg(all(all(test), unix))]",
        ] {
            assert!(is_test_only_cfg(line), "{line}");
        }
        for line in [
            "#[cfg(any(test, feature = \"x\"))]",
            "#[cfg(all(any(test, unix), windows))]",
            "#[cfg(not(test))]",
            "#[cfg(not(all(test, unix)))]",
            "#[cfg(feature = \"testing\")]",
            "#[cfg(unix)]",
            "// gated by #[cfg(test)] somewhere else",
            "mod tests;",
        ] {
            assert!(!is_test_only_cfg(line), "{line}");
        }
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
        let parent = tmp.path().join("m").join("mod.rs");
        assert!(
            msg.contains("mod tests;")
                && msg.contains(&tests.display().to_string())
                && msg.contains(&parent.display().to_string()),
            "message must name the missing item, the module file and its parent: {msg}"
        );
    }

    /// The inline test module starts at a top-level test-only `cfg` gating a
    /// body opened in place — the `all(test, …)` spelling included — and the
    /// production text before it is returned byte-exact.
    #[test]
    fn production_half_stops_at_the_inline_test_module() {
        let src = "fn a() {}\n\n#[cfg(all(test, unix))]\n/// docs\n#[allow(dead_code)]\nmod tests {\n    fn b() {}\n}\n";
        assert_eq!(production_half(src), "fn a() {}\n\n");
    }

    /// A `mod tests;` declaration names a sibling file the name rules
    /// classify; the production items after it stay in the production half.
    #[test]
    fn production_half_keeps_the_items_after_a_sibling_declaration() {
        let src = "#[cfg(test)]\nmod tests;\n\nfn later() {}\n";
        assert_eq!(production_half(src), src);
    }

    /// Neither a non-test `cfg` nor a test `cfg` nested inside another item
    /// starts the inline test module.
    #[test]
    fn production_half_ignores_non_test_and_nested_cfgs() {
        let src = "#[cfg(unix)]\nmod posix {\n    #[cfg(test)]\n    mod inner {}\n}\n#[cfg(not(test))]\nfn prod() {}\n";
        assert_eq!(production_half(src), src);
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
