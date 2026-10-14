//! What counts as a whole test source file, and the premise that makes
//! skipping one by name sound.
//!
//! The `.claude/scripts/**` scanners classify a file as test code by NAME
//! (`lib/test-regions.awk`'s `is_test_file`) because a sibling `tests.rs`,
//! a `<name>_tests.rs` and a `crates/<crate>/tests/**` integration file carry
//! no `#[cfg(test)]` of their own. Rust-side structural scans need the same
//! two answers, and a second spelling of either one drifts: a scan
//! that stops agreeing with the lexer reports on code the scanners skip, or
//! skips code they report on.
//!
//! - [`is_test_source_path`] mirrors `is_test_file` rule for rule.
//! - [`declared_under_test_cfg`] checks the premise behind the name match —
//!   the parent module really does declare the file under a test-only `cfg`.
//! - [`production_half`] finds where a production file's inline test module
//!   starts, so a scan of production code stops before it.
//! - [`rust_sources`] is the directory read itself: every production `.rs` file under a
//!   directory, with test sources skipped by name once their declaration has
//!   been checked. [`test_sources`] is that read's other half — the sources it
//!   skipped, unchecked, for the caller that checks them. Both come from
//!   `partition_sources`, the one directory walk in the workspace.
//! - [`function_bodies`] splits a source into per-function bodies, the grain a
//!   structural guard needs to ask what one function does.
//! - [`workspace_crate_dirs`] lists the workspace's crates, and
//!   [`workspace_production_sources`] is the population a repo-wide
//!   structural test reads: the production half of every one of them. A test
//!   asks for the crate list and never lists `crates/` itself.
//! - [`workspace_sources`] is every `.rs` file the workspace ships, production
//!   and test halves and integration tests alike, for a test that must see all
//!   of them.
//! - [`item_initializers`] splits a source into its `static` and `const`
//!   items outside function bodies, whose initializers run code no body holds.
//! - [`production_corpus`] is [`workspace_production_sources`] read and split
//!   once per test binary, and [`cached_regex`] compiles a pattern once, for
//!   the scans that read the whole workspace.

use std::path::{Path, PathBuf};

/// Every production `.rs` file under `dir`, recursively.
///
/// Test sources are skipped by NAME — whatever [`is_test_source_path`] names,
/// plus a `tests/` module directory. Their content cannot tell, because a
/// sibling test file carries no `#[cfg(test)]` of its own. Skipping one by
/// name is only sound while it really is test-only, so every skipped path is
/// checked against its parent module's declaration, and an ungated one
/// panics, so no production code is dropped from the scan unnoticed.
pub fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let (production, tests) = partition_sources(dir);
    for path in &tests {
        declared_under_test_cfg(path).unwrap_or_else(|why| panic!("{why}"));
    }
    production
}

/// The other half of [`rust_sources`]'s directory read: every test source it skips — a
/// whole test source file by name, and a `tests/` module directory itself,
/// which is the unit a parent module declares.
///
/// Returned without checking the declarations, for the caller whose job is to
/// check them: the two directions come from one directory read, so a rule either learns
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
/// Spelled as a rule. A list of the four commonest prefixes leaves
/// `async fn`, `const fn`, `unsafe fn` and `pub(in …) fn` — all of which this workspace contains — unexamined by every
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

/// Split Rust source into its `static` and `const` items outside every
/// function body: each item from its keyword line to the `;` that ends it,
/// outside brackets and string literals. A `const fn` is a function, so it is
/// left to [`function_bodies`], and so is an item declared inside a body.
///
/// A scan that asks what code computes needs these too: an initializer such
/// as `static T: LazyLock<…> = LazyLock::new(|| …)` runs code no function
/// body holds.
///
/// ```
/// # use anodizer_core::test_helpers::test_sources::item_initializers;
/// let src = "const A: &str = \"x;y\";\nconst fn f() {\n    const B: u8 = 1;\n}\npub static C: [u8; 2] = [\n    1, 2,\n];\n";
/// assert_eq!(
///     item_initializers(src),
///     vec!["const A: &str = \"x;y\";", "pub static C: [u8; 2] = [\n    1, 2,\n];"],
/// );
/// ```
pub fn item_initializers(src: &str) -> Vec<String> {
    let lines: Vec<&str> = src.lines().collect();
    let mut in_fn = vec![false; lines.len()];
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if !opens_fn_item(trimmed) {
            continue;
        }
        let closing = format!("{}}}", " ".repeat(line.len() - trimmed.len()));
        let end = lines[i + 1..]
            .iter()
            .position(|l| *l == closing)
            .map_or(lines.len() - 1, |p| i + 1 + p);
        in_fn[i..=end].iter_mut().for_each(|f| *f = true);
    }
    let mut items = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if in_fn[i] || !opens_value_item(line.trim_start()) {
            continue;
        }
        let text = lines[i..].join("\n");
        let (mut depth, mut in_str, mut prev) = (0usize, false, '\0');
        let mut end = text.len();
        for (at, c) in text.char_indices() {
            match c {
                '"' if prev != '\\' => in_str = !in_str,
                _ if in_str => {}
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth = depth.saturating_sub(1),
                ';' if depth == 0 => {
                    end = at + 1;
                    break;
                }
                _ => {}
            }
            prev = c;
        }
        items.push(text[..end].to_string());
    }
    items
}

/// Whether `trimmed` opens a `static` or `const` item: the keyword, after an
/// optional visibility, followed by a name.
fn opens_value_item(trimmed: &str) -> bool {
    let mut rest = trimmed;
    if let Some(after_pub) = rest.strip_prefix("pub") {
        rest = match after_pub.strip_prefix('(') {
            Some(scoped) => match scoped.split_once(')') {
                Some((_, tail)) => tail,
                None => return false,
            },
            None => after_pub,
        };
        rest = rest.trim_start();
    }
    let Some(tail) = rest
        .strip_prefix("static ")
        .or_else(|| rest.strip_prefix("const "))
    else {
        return false;
    };
    let tail = tail
        .trim_start()
        .strip_prefix("mut ")
        .unwrap_or(tail)
        .trim_start();
    tail.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && !opens_fn_item(trimmed)
        && !tail.starts_with("fn ")
        && !tail.starts_with("unsafe ")
        && !tail.starts_with("async ")
        && !tail.starts_with("extern ")
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
/// test module. That module starts at the run of attribute and comment lines
/// above an item written out in place (`mod tests {` …) when one attribute of
/// the run is a top-level test-only `#[cfg(…)]` ([`is_test_only_cfg`]). The
/// attributes may share the item's line (`#[cfg(test)] mod tests {`) or
/// spread over several lines (`#[cfg(all(` … `))]`). Inside an attribute, a
/// string, raw string, character literal or comment is set aside: its
/// brackets do not open or close the attribute, and a comment between the
/// terms of a `cfg` (`#[cfg(all( // why` … `test))]`) is not part of a term.
/// The module may have a raw name (`mod r#tests {`), a `// …` or `/* … */`
/// comment may follow the `{` or sit between attribute and item, and an
/// attribute such as `#[cfg_attr(test, …)]` in the run is cut with the
/// module. A `mod tests;` declaration does not start one — its body is a
/// file the name rules already classify, and production items follow the
/// declaration — and neither does a `cfg` nested inside another item. A file
/// with no inline test module is returned whole.
///
/// ```
/// # use anodizer_core::test_helpers::test_sources::production_half;
/// let inline = "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn b() {}\n}\n";
/// assert_eq!(production_half(inline), "fn a() {}\n");
/// let one_line = "fn a() {}\n#[cfg(test)] mod tests { // unit tests\n}\n";
/// assert_eq!(production_half(one_line), "fn a() {}\n");
/// let sibling = "#[cfg(test)]\nmod tests;\nfn a() {}\n";
/// assert_eq!(production_half(sibling), sibling);
/// let helper = "#[cfg(test)]\nfn h() {\n}\nfn a() {}\n";
/// assert_eq!(production_half(helper), helper);
/// ```
pub fn production_half(text: &str) -> &str {
    let mut offset = 0usize;
    // Where the current run of attribute and comment lines began, and
    // whether one of its attributes is a test-only `cfg`.
    let mut run_start: Option<usize> = None;
    let mut gated = false;
    let mut in_block_comment = false;
    // An attribute spread over several lines, joined until its brackets
    // close, and the offset of its first line.
    let mut attribute: Option<(usize, String)> = None;
    for line in text.split_inclusive('\n') {
        let line_at = offset;
        offset += line.len();
        let joined;
        let (at, line) = match attribute.take() {
            Some((start, mut text)) => {
                text.push_str(line);
                if !brackets_close(&text) {
                    attribute = Some((start, text));
                    continue;
                }
                joined = text;
                (start, joined.as_str())
            }
            None if !in_block_comment && line.starts_with("#[") && !brackets_close(line) => {
                attribute = Some((line_at, line.to_string()));
                continue;
            }
            None => (line_at, line),
        };
        if in_block_comment {
            in_block_comment = !line.contains("*/");
            continue;
        }
        if line.starts_with("//") {
            run_start.get_or_insert(at);
            continue;
        }
        if let Some(body) = line.strip_prefix("/*") {
            run_start.get_or_insert(at);
            in_block_comment = !body.contains("*/");
            continue;
        }
        if line.trim().is_empty() {
            // A blank line inside a gated run still belongs to the gated
            // item; one before any test `cfg` ends the run.
            if !gated {
                run_start = None;
            }
            continue;
        }
        let item = if line.starts_with("#[") {
            run_start.get_or_insert(at);
            let (attributes_gate, rest) = leading_attributes(line);
            gated |= attributes_gate;
            if rest.is_empty() {
                continue;
            }
            rest
        } else {
            line
        };
        // The gated item itself: a module body opened in place is the inline
        // test module. Any other gated item (a `mod tests;` declaration, a
        // test-only helper `fn`, `impl` or `use` group) is one item among the
        // production ones, and cutting there would hide every production item
        // written after it.
        if gated && is_inline_module_header(item) {
            return &text[..run_start.unwrap_or(at)];
        }
        run_start = None;
        gated = false;
    }
    text
}

/// The attributes that open `line`, read off one at a time: whether any of
/// them is a test-only `cfg` ([`is_test_only_cfg`]), and the code after the
/// last of them with any trailing comment cut and whitespace trimmed (empty
/// when the line holds attributes alone).
fn leading_attributes(line: &str) -> (bool, &str) {
    let mut gated = false;
    let mut rest = line.trim_start();
    while rest.starts_with("#[") {
        let mut depth = 0usize;
        let mut close = None;
        let code = attribute_code(rest);
        for &(index, ch) in code.iter().skip(1) {
            match ch {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(index);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(close) = close else {
            return (gated, "");
        };
        // The predicate is read with its comments and literals left out, so
        // a `// why` between terms is not part of the `test` term.
        let attribute: String = code
            .iter()
            .take_while(|&&(index, _)| index <= close)
            .map(|&(_, ch)| ch)
            .collect();
        gated |= is_test_only_cfg(&attribute);
        rest = rest[close + 1..].trim_start();
    }
    (gated, strip_trailing_comment(rest).trim())
}

/// `line` with a trailing `// …` or `/* … */` comment cut.
fn strip_trailing_comment(line: &str) -> &str {
    let cut = [line.find("//"), line.find("/*")]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(line.len());
    &line[..cut]
}

/// `mod name {`, `mod r#name {` or `pub(…) mod name {`, a trailing comment
/// allowed: a module whose body is written in place. `mod a; mod b {` is the
/// declaration `mod a;` first, so it is not one.
fn is_inline_module_header(line: &str) -> bool {
    let item = strip_trailing_comment(line).trim();
    let item = match item.strip_prefix("pub") {
        Some(rest) if rest.starts_with('(') => rest.split_once(')').map_or(rest, |(_, r)| r),
        Some(rest) => rest,
        None => item,
    };
    item.trim_start()
        .strip_prefix("mod ")
        .and_then(|rest| rest.trim_end().strip_suffix('{'))
        .is_some_and(|name| {
            let name = name.trim();
            let name = name.strip_prefix("r#").unwrap_or(name);
            !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_')
        })
}

/// Whether every `[` in `text` has its `]`, so an attribute that opens in
/// it has ended. Brackets inside a literal or a comment are text.
fn brackets_close(text: &str) -> bool {
    attribute_code(text)
        .into_iter()
        .map(|(_, ch)| match ch {
            '[' => 1,
            ']' => -1,
            _ => 0,
        })
        .sum::<i32>()
        <= 0
}

/// The characters of attribute text, with their offsets, leaving out each
/// `"…"` string, `r#"…"#` raw string (`br` and `cr` ones included), `'x'`
/// character literal, `//` comment up to its line's end and `/* … */`
/// comment (nested ones included).
fn attribute_code(text: &str) -> Vec<(usize, char)> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let at = |i: usize| chars.get(i).map(|&(_, ch)| ch);
    let mut code = Vec::new();
    let mut i = 0;
    while let Some(ch) = at(i) {
        let next = at(i + 1);
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        let word_before = i > 0 && at(i - 1).is_some_and(is_word);
        // The `b` of `br"…"` or the `c` of `cr"…"`, itself starting a token.
        let raw_prefix = |i: usize| {
            i > 0
                && matches!(at(i - 1), Some('b' | 'c'))
                && !(i > 1 && at(i - 2).is_some_and(is_word))
        };
        if ch == '/' && next == Some('/') {
            while at(i).is_some_and(|c| c != '\n') {
                i += 1;
            }
        } else if ch == '/' && next == Some('*') {
            let mut depth = 0usize;
            while let Some(c) = at(i) {
                if c == '/' && at(i + 1) == Some('*') {
                    depth += 1;
                    i += 2;
                } else if c == '*' && at(i + 1) == Some('/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
        } else if ch == '"' {
            i += 1;
            while let Some(c) = at(i) {
                i += if c == '\\' { 2 } else { 1 };
                if c == '"' {
                    break;
                }
            }
        } else if let Some(hashes) = (ch == 'r' && (!word_before || raw_prefix(i)))
            .then(|| (i + 1..).take_while(|&j| at(j) == Some('#')).count())
            .filter(|&hashes| at(i + 1 + hashes) == Some('"'))
        {
            let close: String = std::iter::once('"')
                .chain("#".repeat(hashes).chars())
                .collect();
            let body = i + 2 + hashes;
            let rest: String = chars[body.min(chars.len())..]
                .iter()
                .map(|&(_, c)| c)
                .collect();
            i = match rest.find(&close) {
                Some(end) => body + rest[..end].chars().count() + close.chars().count(),
                None => chars.len(),
            };
        } else if ch == '\'' && next == Some('\\') {
            i += 3;
            while at(i).is_some_and(|c| c != '\'') {
                i += 1;
            }
            i += 1;
        } else if ch == '\'' && next.is_some() && at(i + 2) == Some('\'') {
            i += 3;
        } else {
            code.push(chars[i]);
            i += 1;
        }
    }
    code
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

/// Every crate directory in this workspace.
///
/// The one spelling of the `crates/` listing. Two copies disagree the moment
/// one of them learns a filter the other does not, and six had accumulated.
pub fn workspace_crate_dirs() -> Vec<PathBuf> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/ above crates/core");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(crates)
        .expect("crates dir")
        .map(|entry| entry.expect("crate entry").path())
        .collect();
    dirs.sort();
    dirs
}

/// The production half of every workspace crate's `src/`.
///
/// The population a repo-wide structural test reads. Enumerating `crates/`
/// per test had drifted once already — one copy listed the directory and
/// another parsed the manifest's `members`, so a crate directory that is not
/// a member was in one population and not the other. The minimum count here
/// fails a test that would otherwise pass vacuously over an empty corpus.
pub fn workspace_production_sources() -> Vec<PathBuf> {
    let mut sources: Vec<PathBuf> = Vec::new();
    for krate in workspace_crate_dirs() {
        let src = krate.join("src");
        if src.is_dir() {
            sources.extend(rust_sources(&src));
        }
    }
    assert!(
        sources.len() > 100,
        "the corpus must cover every crate's production sources, found {}",
        sources.len()
    );
    sources
}

/// Every `.rs` file the workspace ships, production and test alike,
/// reached through the shared directory read itself. An integration-test directory
/// is only expanded: its files are declared by cargo's target layout, not
/// by a parent module, so the declaration check does not apply to them.
pub fn workspace_sources() -> Vec<PathBuf> {
    fn expand(dir: &Path, out: &mut Vec<PathBuf>) {
        for path in test_sources(dir) {
            if path.is_dir() {
                expand(&path, out);
            } else {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    for krate in workspace_crate_dirs() {
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
            // declared by cargo and has no `mod` item.
            out.extend(partition_sources(&tests).0);
            expand(&tests, &mut out);
        }
    }
    assert!(!out.is_empty(), "the workspace ships no source");
    out
}

/// One production source of the workspace, read and split once.
pub struct ProductionSource {
    /// The file's path.
    pub path: PathBuf,
    /// Its production half ([`production_half`]).
    pub text: String,
    /// That half split into function bodies ([`function_bodies`]).
    pub bodies: Vec<String>,
    /// That half's `static` and `const` items ([`item_initializers`]).
    pub items: Vec<String>,
}

/// Every production source of the workspace ([`workspace_production_sources`])
/// with its production half, function bodies and items, read and split once
/// per test binary.
///
/// Every structural scan that reads the whole workspace asks the same
/// question of the same files, so the read and the split are shared: two
/// scans that each read the workspace took a minute between them.
pub fn production_corpus() -> &'static [ProductionSource] {
    static CORPUS: std::sync::LazyLock<Vec<ProductionSource>> = std::sync::LazyLock::new(|| {
        workspace_production_sources()
            .into_iter()
            .map(|path| {
                let whole = std::fs::read_to_string(&path).expect("readable source");
                let text = production_half(&whole).to_string();
                let bodies = function_bodies(&text);
                let items = item_initializers(&text);
                ProductionSource {
                    path,
                    text,
                    bodies,
                    items,
                }
            })
            .collect()
    });
    &CORPUS
}

/// `pattern` compiled once for the whole test binary.
///
/// A structural scan asks the same few patterns of every function body in
/// the workspace, and a shared `Regex` keeps its match caches warm between
/// bodies, where a fresh copy would rebuild them on every search.
pub fn cached_regex(pattern: impl Into<String>) -> &'static regex::Regex {
    static CACHE: std::sync::LazyLock<
        std::sync::Mutex<std::collections::BTreeMap<String, &'static regex::Regex>>,
    > = std::sync::LazyLock::new(Default::default);
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let pattern = pattern.into();
    if let Some(re) = cache.get(&pattern) {
        return re;
    }
    let re: &'static regex::Regex =
        Box::leak(Box::new(regex::Regex::new(&pattern).expect("valid regex")));
    cache.insert(pattern, re);
    re
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`partition_sources`] is the only directory walk that picks Rust files
    /// out of a tree. A second one drifts from it unnoticed: a rule taught to
    /// one directory read and not the other reports on code the scanners
    /// skip, or skips code they report on, and two such copies had already drifted before
    /// this guard existed. Any function that both reads a directory and tests
    /// for the Rust file extension fails here until it goes through this module.
    /// Whether a function body is a Rust-source directory walk: it reads a
    /// directory AND either tests for the extension or feeds what it read to
    /// one of this module's scanners. Both extension spellings count — an
    /// `extension() == "rs"` compare and a `ends_with(".rs")` one — because a
    /// detector that knows only the first is blind to the second, which
    /// contains no `"rs"` token at all. The scanner-call clause is what sees
    /// a test listing `crates/` itself and handing each `src/` to
    /// [`rust_sources`]: five of those had accumulated, and none of them
    /// names a file extension.
    fn is_rust_source_walk(body: &str) -> bool {
        body.contains("read_dir(")
            && (body.contains("\"rs\"")
                || body.contains("\".rs\"")
                || body.contains("rust_sources(")
                || body.contains("test_sources("))
    }

    /// The renderer internals a documented-render test needs are re-exported
    /// only under the `test-helpers` feature. Ungated they are permanent
    /// public API of a consumer-facing library, and un-promising them later is
    /// a breaking change — the cheaper direction is not to promise them.
    #[test]
    fn the_renderer_internals_are_re_exported_only_under_the_test_feature() {
        let mod_rs = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/log/mod.rs");
        let text = std::fs::read_to_string(&mod_rs).expect("readable log module");
        // Read the whole `pub use` statement: rustfmt
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
                defs.push(crate::path_util::slash_display(&source));
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

    /// The directory-read detector must see both ways a source names the extension. A
    /// `.ends_with(".rs")` walker contains no `"rs"` token, so a detector
    /// spelled only the first way lets it through with a passing suite.
    #[test]
    fn the_walk_detector_reads_both_extension_spellings() {
        assert!(is_rust_source_walk(
            "fn a(d: &Path) { for e in std::fs::read_dir(d)? { if e.path().extension() == Some(\"rs\".as_ref()) {} } }"
        ));
        assert!(is_rust_source_walk(
            "fn b(d: &Path) { for e in std::fs::read_dir(d)? { if e.path().to_string_lossy().ends_with(\".rs\") {} } }"
        ));
        // A crate listing that hands each `src/` to a shared scanner never
        // names an extension, and is the shape five tests had re-spelled.
        assert!(is_rust_source_walk(
            "fn c(d: &Path) { for e in std::fs::read_dir(d)? { out.extend(rust_sources(&e)); } }"
        ));
        // A directory read that neither asks about the extension nor collects
        // sources is not a Rust-source directory read.
        assert!(!is_rust_source_walk(
            "fn d(d: &Path) { std::fs::read_dir(d) }"
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

    /// Whether a function body lists the workspace's production sources and
    /// cuts each one to its production half itself. [`production_corpus`]
    /// does both once per test binary; a scan that repeats them reads and
    /// splits every file of the workspace again, which cost two scans a
    /// minute between them.
    fn splits_the_production_sources_itself(body: &str) -> bool {
        body.contains("workspace_production_sources(") && body.contains("production_half(")
    }

    /// A body that lists the production sources and cuts each one itself is
    /// reported, and one that iterates the corpus is not.
    #[test]
    fn the_corpus_detector_reports_a_scan_that_splits_the_sources_itself() {
        assert!(splits_the_production_sources_itself(
            "fn a() { for p in workspace_production_sources() { let t = read(&p); scan(production_half(&t)); } }"
        ));
        assert!(!splits_the_production_sources_itself(
            "fn b() { for source in production_corpus() { scan(&source.text); } }"
        ));
        // Listing the paths alone, without cutting each file, is a different
        // question the corpus does not answer.
        assert!(!splits_the_production_sources_itself(
            "fn c() { let n = workspace_production_sources().len(); }"
        ));
    }

    #[test]
    fn every_rust_source_walk_comes_from_the_shared_scanner() {
        let mut walks = Vec::new();
        let mut private_splits = Vec::new();
        for source in workspace_sources() {
            let text = std::fs::read_to_string(&source).expect("readable source");
            for body in function_bodies(&text) {
                let walk = is_rust_source_walk(&body);
                let split = splits_the_production_sources_itself(&body);
                if !walk && !split {
                    continue;
                }
                let name = body
                    .split_once("fn ")
                    .and_then(|(_, rest)| rest.split(['(', '<']).next())
                    .unwrap_or_default()
                    .to_string();
                let site = format!("{}: {name}", crate::path_util::slash_display(&source));
                if split {
                    private_splits.push(site.clone());
                }
                if walk {
                    walks.push(site);
                }
            }
        }
        // `production_corpus` builds the corpus, and the detector and its
        // fixture test carry the needles the detector looks for.
        const CORPUS_ALLOWED: [&str; 3] = [
            "core/src/test_helpers/test_sources.rs: production_corpus",
            "core/src/test_helpers/test_sources.rs: splits_the_production_sources_itself",
            "core/src/test_helpers/test_sources.rs: the_corpus_detector_reports_a_scan_that_splits_the_sources_itself",
        ];
        let mut found: Vec<String> = private_splits
            .iter()
            .map(|w| {
                CORPUS_ALLOWED
                    .iter()
                    .find(|a| w.ends_with(*a))
                    .map_or_else(|| w.clone(), |a| (*a).to_string())
            })
            .collect();
        found.sort();
        let mut expected: Vec<String> = CORPUS_ALLOWED.iter().map(|a| (*a).to_string()).collect();
        expected.sort();
        assert_eq!(
            found, expected,
            "a scan that lists `workspace_production_sources` and cuts each file \
             with `production_half` itself: iterate `production_corpus` and read \
             `source.path`, `source.text` and `source.bodies`"
        );
        // `crate_has_binary_target` lists one crate's `src/bin` to answer
        // whether a binary target exists at all; it collects no source to
        // scan, so it is not a directory read this module can serve.
        // `is_rust_source_walk` is the detector and its own test fixture
        // carries the needles it looks for, so both match themselves.
        const ALLOWED: [&str; 4] = [
            "core/src/test_helpers/test_sources.rs: is_rust_source_walk",
            "core/src/test_helpers/test_sources.rs: partition_sources",
            "core/src/test_helpers/test_sources.rs: the_walk_detector_reads_both_extension_spellings",
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
            "a Rust-source directory read outside the shared scanner: it must take its \
             files from `rust_sources` / `test_sources`"
        );
    }

    /// The directory read skips a `tests/` module directory only while its
    /// parent declares it under a test-only `cfg`: gated, the read yields
    /// only the parent module; ungated, the read fails, so the production
    /// code it would have scanned is never dropped unnoticed.
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
            "an ungated tests/ directory must fail the directory read"
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

    /// The inline test module is found in each of the shapes it is written
    /// in: attribute and header on one line, a comment after the `{`, a
    /// block comment between attribute and header, a `cfg_attr(test, …)`
    /// attribute in the run, which is cut with the module, and an attribute
    /// spread over several lines. A declaration that shares the line with a
    /// production module is not one.
    #[test]
    fn production_half_finds_every_shape_of_the_inline_test_module() {
        let prod = "fn a() {}\n\n";
        for module in [
            "#[cfg(test)] mod tests {\n    fn b() {}\n}\n",
            "#[cfg(test)]\nmod tests { // unit tests\n    fn b() {}\n}\n",
            "#[cfg(test)]\nmod tests { /* unit tests */\n}\n",
            "#[cfg(test)]\n/* the tests\n   for a */\nmod tests {\n}\n",
            "#[cfg(test)]\n/* the tests for a */\nmod tests {\n}\n",
            "#[cfg_attr(test, allow(dead_code))]\n#[cfg(test)]\nmod tests {\n}\n",
            "#[cfg(test)]\n#[cfg_attr(test, allow(dead_code))]\nmod tests {\n}\n",
            "#[allow(dead_code)] #[cfg(all(test, unix))] pub(crate) mod tests {\n}\n",
            "#[cfg(test)]\n\nmod tests {\n}\n",
            "#[cfg(all(\n    test,\n    unix\n))]\nmod tests {\n}\n",
            "#[cfg(all(\n    test,\n    unix\n))] mod tests {\n}\n",
            "#[allow(dead_code)] // ranges like [a, b)\n#[cfg(test)]\nmod tests {\n}\n",
            "#[cfg_attr(test, doc = \"[\")]\n#[cfg(test)]\nmod tests {\n}\n",
            "#[cfg(all(\n    feature = \"a]\",\n    test\n))]\nmod tests {\n}\n",
            "#[cfg(test)]\nmod r#tests {\n}\n",
            "#[cfg(all( // why\n    test, unix))]\nmod tests {\n}\n",
            "#[cfg(all( /* why, [ */ test, unix))]\nmod tests {\n}\n",
            "#[my_attr('\"')]\n#[cfg(test)]\nmod tests {\n}\n",
            "#[my_attr('[')]\n#[cfg(test)]\nmod tests {\n}\n",
            "#[my_attr(']', '\\'')]\n#[cfg(test)]\nmod tests {\n}\n",
            "#[doc = r#\"a \" ] b\"#]\n#[cfg(test)]\nmod tests {\n}\n",
            "#[doc = r\"[\"]\n#[cfg(test)]\nmod tests {\n}\n",
            "#[my_attr(br\"\\\")]\n#[cfg(test)]\nmod tests {\n}\n",
            "#[my_attr(br#\"q \" ] \"#)]\n#[cfg(test)]\nmod tests {\n}\n",
            "#[my_attr(cr\"\\\")]\n#[cfg(test)]\nmod tests {\n}\n",
            "#[my_attr(cr#\"q \" ] \"#)]\n#[cfg(test)]\nmod tests {\n}\n",
        ] {
            let src = format!("{prod}{module}");
            assert_eq!(production_half(&src), prod, "{module}");
        }
        for not_a_module in [
            "#[cfg(test)] mod a; mod prod {\n}\n",
            "#[cfg_attr(test, allow(dead_code))]\nmod prod {\n}\n",
            "#[cfg(test)] mod tests;\nfn later() {}\n",
            "#[cfg(test)] fn helper() {}\nmod later {\n}\n",
            "// #[cfg(test)]\nmod prod {\n}\n",
            "#[cfg_attr(test, doc = \"[\")]\nfn prod() {}\n#[cfg(test)]\nmod r#a b {\n}\n",
            "#[cfg(all(unix /* , test */))]\nmod prod {\n}\n",
            "#[cfg(all(unix, // test\n))]\nmod prod {\n}\n",
            "#[my_attr('\"')]\nfn prod() {}\n",
        ] {
            let src = format!("{prod}{not_a_module}");
            assert_eq!(production_half(&src), src, "{not_a_module}");
        }
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

    /// `production_half` stops at the first inline test module, so a
    /// production item written after one is invisible to every structural
    /// test that reads the production half. Each production source keeps
    /// its inline test modules at the end, after the last production item.
    #[test]
    fn no_production_item_follows_an_inline_test_module() {
        const ITEM_STARTS: &[&str] = &[
            "pub ",
            "pub(",
            "fn ",
            "async fn ",
            "const ",
            "static ",
            "struct ",
            "enum ",
            "impl ",
            "impl<",
            "trait ",
            "type ",
            "unsafe ",
            "macro_rules!",
            "use ",
            "mod ",
        ];
        let mut stranded = Vec::new();
        for source in production_corpus() {
            let path = source.path.to_string_lossy().replace('\\', "/");
            // The helpers are test code compiled under a feature, and their
            // own fixtures hold column-0 items inside string literals.
            if path.contains("/test_helpers/") {
                continue;
            }
            // The corpus holds only the production half, and this scan reads
            // what follows it. The file is cut again from this one read so a
            // file edited since the corpus was read is still cut where its
            // own test module starts.
            let text = std::fs::read_to_string(&source.path).expect("read source");
            let tail = &text[production_half(&text).len()..];
            let lines: Vec<&str> = tail.lines().collect();
            let mut in_block_comment = false;
            for (index, line) in lines.iter().enumerate() {
                if in_block_comment {
                    in_block_comment = !line.contains("*/");
                    continue;
                }
                if let Some(body) = line.strip_prefix("/*") {
                    in_block_comment = !body.contains("*/");
                    continue;
                }
                let (same_line_gate, item) = if line.starts_with("#[") {
                    leading_attributes(line)
                } else {
                    (false, *line)
                };
                if !ITEM_STARTS.iter().any(|start| item.starts_with(start)) {
                    continue;
                }
                let gated = same_line_gate
                    || lines[..index]
                        .iter()
                        .rev()
                        .take_while(|above| {
                            ["#[", "//", "/*", " *", "*/"]
                                .iter()
                                .any(|start| above.starts_with(start))
                        })
                        .any(|above| above.starts_with("#[") && leading_attributes(above).0);
                if !gated {
                    stranded.push(format!("{path}: {line}"));
                }
            }
        }
        assert!(
            stranded.is_empty(),
            "move the inline test module above these items to the end of its file: {stranded:#?}"
        );
    }
}
