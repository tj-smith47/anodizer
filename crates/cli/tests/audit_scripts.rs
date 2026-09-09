//! What the audit scanners treat as test code.
//!
//! The scanners under `.claude/scripts/` share one test-region lexer
//! (`lib/test-regions.awk`) with two polarities: `audit-binary-name.sh` and
//! `audit-tag-family.sh` SKIP test code, `audit-test-isolation.sh` and
//! `audit-test-spawn-retry.sh` report only INSIDE it. A regression in that
//! lexer is silent — a scanner keeps exiting 0 while it has stopped looking at
//! half the tree — so each test here runs a scanner over a fixture tree
//! carrying the shapes that broke it before and asserts the whole hit list:
//!
//! * a raw read placed AFTER an inline `#[cfg(test)] mod … { … }` is
//!   production and is reported;
//! * a second attribute between `#[cfg(test)]` and `mod` still opens a region,
//!   so the reads inside it are not;
//! * a process-global mutation or a `git` spawn inside such a module is test
//!   code and is reported, while the same call after the module is not;
//! * a `crates/*/tests/**` integration file and a sibling `tests.rs` or
//!   `<name>_tests.rs` (no `#[cfg(test)]` of their own) are test code in
//!   their entirety.
//!
//! Treating those siblings as test code by NAME is sound only while every
//! such file really is declared under `#[cfg(test)]` by its parent module;
//! `every_named_test_file_is_declared_cfg_test` walks the real tree for that
//! premise.
#![cfg(unix)]

use std::path::Path;
use std::process::Command;

use anodizer_core::test_helpers::test_sources::{
    declared_under_test_cfg, is_test_only_cfg, is_test_source_path,
};

use tempfile::TempDir;

const LIB_RS: &str = include_str!("fixtures/audit_scripts/lib.rs.txt");
const ATTR_GAP_RS: &str = include_str!("fixtures/audit_scripts/attr_gap.rs.txt");
const REGISTRY_RS: &str = include_str!("fixtures/audit_scripts/registry.rs.txt");
const INTEGRATION_RS: &str = include_str!("fixtures/audit_scripts/integration.rs.txt");
const TESTS_RS: &str = include_str!("fixtures/audit_scripts/tests.rs.txt");
const NAMED_TESTS_RS: &str = include_str!("fixtures/audit_scripts/named_tests.rs.txt");

/// The fixture sources are `.txt` so the workspace's own audits, which scan
/// `*.rs`, never read them as real source.
fn fixture_tree() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    for (rel, body) in [
        ("crates/demo/src/lib.rs", LIB_RS),
        ("crates/demo/src/attr_gap.rs", ATTR_GAP_RS),
        ("crates/demo/tests/spawn.rs", INTEGRATION_RS),
        ("crates/demo/src/tests.rs", TESTS_RS),
        ("crates/demo/src/named_tests.rs", NAMED_TESTS_RS),
        ("crates/core/src/artifact/registry.rs", REGISTRY_RS),
    ] {
        let path = dir.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).expect("fixture dir");
        std::fs::write(&path, body).expect("fixture file");
    }
    dir
}

fn run_audit(script: &str, root: &Path) -> (i32, String) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(".claude/scripts")
        .join(script);
    let out = Command::new("bash")
        .arg(&path)
        .arg(root)
        .output()
        .unwrap_or_else(|e| panic!("running {}: {e}", path.display()));
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), text)
}

/// Every scanner prints its findings as `<crate-relative path>:<line>…`; the
/// rest of the output is the shared remediation prose.
fn hits(output: &str) -> Vec<String> {
    let mut found: Vec<String> = output
        .lines()
        .filter(|l| l.starts_with("crates/") && l.contains(".rs:"))
        .map(str::to_string)
        .collect();
    found.sort();
    found
}

fn at(src: &str, needle: &str) -> (usize, String) {
    let (index, line) = src
        .lines()
        .enumerate()
        .find(|(_, l)| l.contains(needle))
        .unwrap_or_else(|| panic!("the fixture no longer contains `{needle}`"));
    (index + 1, line.trim().to_string())
}

#[test]
fn binary_name_audit_reads_production_after_an_inline_test_module() {
    let dir = fixture_tree();
    let (code, out) = run_audit("audit-binary-name.sh", dir.path());

    let (line, text) = at(LIB_RS, "let production_binary");
    assert_eq!(
        hits(&out),
        vec![format!(
            "crates/demo/src/lib.rs:{line} (fn after_the_inline_module): {text}"
        )],
        "{out}"
    );
    assert_eq!(code, 1, "{out}");
}

#[test]
fn tag_family_audit_reads_production_after_an_inline_test_module() {
    let dir = fixture_tree();
    let (code, out) = run_audit("audit-tag-family.sh", dir.path());

    let (line, text) = at(LIB_RS, "let _production_family");
    assert_eq!(
        hits(&out),
        vec![format!(
            "crates/demo/src/lib.rs:{line} (fn after_the_inline_module): {text}"
        )],
        "{out}"
    );
    assert_eq!(code, 1, "{out}");
}

#[test]
fn test_isolation_audit_reports_test_code_only() {
    let dir = fixture_tree();
    let (code, out) = run_audit("audit-test-isolation.sh", dir.path());

    let (inline_line, inline_text) = at(LIB_RS, "INLINE_ONLY");
    let (sibling_line, sibling_text) = at(TESTS_RS, "SIBLING_FILE");
    let (named_line, named_text) = at(NAMED_TESTS_RS, "NAMED_SIBLING_FILE");
    let (file_line, file_text) = at(INTEGRATION_RS, "INTEGRATION_FILE");
    assert_eq!(
        hits(&out),
        vec![
            format!("crates/demo/src/lib.rs:{inline_line}: [env] {inline_text}"),
            format!("crates/demo/src/named_tests.rs:{named_line}: [env] {named_text}"),
            format!("crates/demo/src/tests.rs:{sibling_line}: [env] {sibling_text}"),
            format!("crates/demo/tests/spawn.rs:{file_line}: [env] {file_text}"),
        ],
        "{out}"
    );
    assert_eq!(code, 1, "{out}");
}

#[test]
fn spawn_retry_audit_reports_test_context_only() {
    let dir = fixture_tree();
    let (code, out) = run_audit("audit-test-spawn-retry.sh", dir.path());

    let (inline_line, inline_text) = at(LIB_RS, r#"arg("status")"#);
    let (file_line, file_text) = at(INTEGRATION_RS, r#"arg("init")"#);
    assert_eq!(
        hits(&out),
        vec![
            format!("crates/demo/src/lib.rs:{inline_line}: {inline_text}"),
            format!("crates/demo/tests/spawn.rs:{file_line}: {file_text}"),
        ],
        "{out}"
    );
    assert_eq!(code, 1, "{out}");
}

/// `audit-test-exec-writer.sh` has to see EVERY spelling of an executable
/// mode — `PermissionsExt`'s `Permissions::from_mode(0o755)` argument and
/// `perms.set_mode(0o755)` mutation, and the `.mode(0o755)` builder call of
/// `OpenOptionsExt`/`DirBuilderExt` — because a call site picks any of them
/// freely and each one the audit could not see was a writer it waved through.
/// It must also stay off production chmods (a stage staging a real binary into
/// a package tree, a production `DirBuilder` mode) and off a mode carrying an
/// `exec-writer-ok:` marker. (Spelled without the comment slashes so this
/// sentence cannot arm the scanner's own marker rule.)
#[test]
fn exec_writer_audit_reports_every_mode_spelling_in_test_context_only() {
    let dir = fixture_tree();
    let (code, out) = run_audit("audit-test-exec-writer.sh", dir.path());

    let (set_mode_line, set_mode_text) = at(LIB_RS, "perms.set_mode");
    let (from_mode_line, from_mode_text) = at(TESTS_RS, r#""sibling-stub""#);
    let (builder_line, builder_text) = at(TESTS_RS, r#""opts-stub""#);
    assert_eq!(
        hits(&out),
        vec![
            format!("crates/demo/src/lib.rs:{set_mode_line}: {set_mode_text}"),
            format!("crates/demo/src/tests.rs:{from_mode_line}: {from_mode_text}"),
            format!("crates/demo/src/tests.rs:{builder_line}: {builder_text}"),
        ],
        "{out}"
    );
    assert_eq!(code, 1, "{out}");
}

/// The premise behind `is_test_file`'s name match: every `tests.rs` and
/// `<name>_tests.rs` under `crates/*/src` is declared `mod <stem>;` under a
/// test-only `cfg` by its parent module (`mod.rs`/`lib.rs`/`main.rs` beside
/// it, or the 2018-layout `<dir>.rs`), with the attribute on the item's line
/// or in the contiguous run of attribute and comment lines directly above it.
/// A file that matches the name but is compiled into production would be
/// skipped by the production-only scanners and reported by the test-only
/// ones — both wrong.
#[test]
fn every_named_test_file_is_declared_cfg_test() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut undeclared = Vec::new();
    let mut seen = 0usize;
    for src in std::fs::read_dir(&crates)
        .expect("crates dir")
        .map(|e| e.expect("crate entry").path().join("src"))
        .filter(|src| src.is_dir())
    {
        for file in test_source_files(&src) {
            seen += 1;
            if let Err(why) = declared_under_test_cfg(&file) {
                undeclared.push(why);
            }
        }
    }
    assert!(
        seen > 0,
        "no name-matched test file under {}",
        crates.display()
    );
    assert!(
        undeclared.is_empty(),
        "name-matched test files not declared `#[cfg(test)] mod …;` by their parent: {undeclared:?}"
    );
}

/// The awk lexer's `is_test_file` and the Rust `is_test_source_path` answer
/// the same question for two families of consumer — the shell scanners and
/// the crates' structural walks. Feed both the same paths and compare
/// verdicts: a rule changed on one side alone fails here.
const AGREEMENT_PATHS: &[&str] = &[
    "crates/demo/src/tests.rs",
    "crates/demo/src/foo_tests.rs",
    "crates/demo/src/_tests.rs",
    "crates/demo/src/mytests.rs",
    "crates/demo/src/Foo_tests.rs",
    "crates/demo/src/tests.rs.bak",
    "crates/demo/src/lib.rs",
    "crates/demo/src/process/tests/mod.rs",
    "crates/demo/tests/integration.rs",
    "crates/demo/tests/nested/case.rs",
    "crates/tests/tests/case.rs",
    "src/tests/helper.rs",
    "tests.rs",
    "foo_tests.rs",
];

#[test]
fn test_source_predicate_agrees_with_the_awk_lexer() {
    let lib = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(".claude/scripts/lib");
    let dir = TempDir::new().expect("temp dir");
    let driver = dir.path().join("driver.awk");
    std::fs::write(&driver, "{ print (is_test_file($0) ? \"1\" : \"0\") }\n").expect("driver");
    let list = dir.path().join("paths.txt");
    std::fs::write(&list, format!("{}\n", AGREEMENT_PATHS.join("\n"))).expect("path list");

    let out = Command::new("awk")
        .arg("-f")
        .arg(lib.join("rust-lex.awk"))
        .arg("-f")
        .arg(lib.join("test-regions.awk"))
        .arg("-f")
        .arg(&driver)
        .arg(&list)
        .output()
        .expect("running awk");
    assert!(
        out.status.success(),
        "awk failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let awk: Vec<&str> = std::str::from_utf8(&out.stdout)
        .expect("awk output is utf-8")
        .lines()
        .collect();
    assert_eq!(
        awk.len(),
        AGREEMENT_PATHS.len(),
        "awk answered {} of {} paths",
        awk.len(),
        AGREEMENT_PATHS.len()
    );
    let disagreements: Vec<String> = AGREEMENT_PATHS
        .iter()
        .zip(&awk)
        .map(|(path, verdict)| (path, *verdict == "1", is_test_source_path(Path::new(path))))
        .filter(|(_, awk_says, rust_says)| awk_says != rust_says)
        .map(|(path, awk_says, rust_says)| format!("{path}: awk={awk_says} rust={rust_says}"))
        .collect();
    assert!(
        disagreements.is_empty(),
        "is_test_file and is_test_source_path must agree: {disagreements:?}"
    );
}

/// The `cfg(…)` predicates whose test-only verdict both lexers must share.
/// Every accepted shape, every rejection the accepted ones are one token away
/// from, and the two non-attribute lines that must never be mistaken for a
/// gate.
const AGREEMENT_CFG_LINES: &[&str] = &[
    "#[cfg(test)]",
    "  #[cfg(test)] mod tests;",
    "#[cfg(all(test, unix))]",
    "#[cfg(all(test, not(windows)))]",
    "#[cfg(all(feature = \"x\", test))]",
    "#[cfg(all(all(test), unix))]",
    "#[cfg(any(test, feature = \"x\"))]",
    "#[cfg(all(any(test, unix), windows))]",
    "#[cfg(not(test))]",
    "#[cfg(not(all(test, unix)))]",
    "#[cfg(feature = \"testing\")]",
    "#[cfg(unix)]",
    "// gated by #[cfg(test)] somewhere else",
    "mod tests;",
];

/// The awk lexer's `is_test_only_cfg` and the Rust `is_test_only_cfg` decide
/// which `cfg(…)` predicates gate test code — the shell scanners bound their
/// test regions with one, the crates' structural walks check the premise
/// behind a name match with the other. A rule loosened on one side alone (an
/// `any(…)` counted as a gate, a `not(…)` term rejected beside a `test` one)
/// lets a production module be skipped as test code, or the reverse. Feed
/// both the same predicate lines and compare verdicts.
#[test]
fn test_only_cfg_predicate_agrees_with_the_awk_lexer() {
    let lib = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(".claude/scripts/lib");
    let dir = TempDir::new().expect("temp dir");
    let driver = dir.path().join("driver.awk");
    std::fs::write(
        &driver,
        "{ print (is_test_only_cfg($0) ? \"1\" : \"0\") }\n",
    )
    .expect("driver");
    let list = dir.path().join("cfg-lines.txt");
    std::fs::write(&list, format!("{}\n", AGREEMENT_CFG_LINES.join("\n"))).expect("cfg lines");

    let out = Command::new("awk")
        .arg("-f")
        .arg(lib.join("rust-lex.awk"))
        .arg("-f")
        .arg(&driver)
        .arg(&list)
        .output()
        .expect("running awk");
    assert!(
        out.status.success(),
        "awk failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let awk: Vec<&str> = std::str::from_utf8(&out.stdout)
        .expect("awk output is utf-8")
        .lines()
        .collect();
    assert_eq!(
        awk.len(),
        AGREEMENT_CFG_LINES.len(),
        "awk answered {} of {} predicate lines",
        awk.len(),
        AGREEMENT_CFG_LINES.len()
    );
    let disagreements: Vec<String> = AGREEMENT_CFG_LINES
        .iter()
        .zip(&awk)
        .map(|(line, verdict)| (line, *verdict == "1", is_test_only_cfg(line)))
        .filter(|(_, awk_says, rust_says)| awk_says != rust_says)
        .map(|(line, awk_says, rust_says)| format!("{line}: awk={awk_says} rust={rust_says}"))
        .collect();
    assert!(
        disagreements.is_empty(),
        "the two `is_test_only_cfg` spellings must agree: {disagreements:?}"
    );
}

/// Every whole test source file under `dir`, recursively — whatever
/// `is_test_source_path` (and so `lib/test-regions.awk`'s `is_test_file`)
/// names.
fn test_source_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).expect("read dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            found.extend(test_source_files(&path));
        } else if is_test_source_path(&path) {
            found.push(path);
        }
    }
    found
}
