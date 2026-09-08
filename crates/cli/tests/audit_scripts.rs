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
//! * a `crates/*/tests/**` integration file and a sibling `tests.rs` (no
//!   `#[cfg(test)]` of their own) are test code in their entirety.
#![cfg(unix)]

use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

const LIB_RS: &str = include_str!("fixtures/audit_scripts/lib.rs.txt");
const ATTR_GAP_RS: &str = include_str!("fixtures/audit_scripts/attr_gap.rs.txt");
const REGISTRY_RS: &str = include_str!("fixtures/audit_scripts/registry.rs.txt");
const INTEGRATION_RS: &str = include_str!("fixtures/audit_scripts/integration.rs.txt");
const TESTS_RS: &str = include_str!("fixtures/audit_scripts/tests.rs.txt");

/// The fixture sources are `.txt` so the workspace's own audits, which scan
/// `*.rs`, never read them as real source.
fn fixture_tree() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    for (rel, body) in [
        ("crates/demo/src/lib.rs", LIB_RS),
        ("crates/demo/src/attr_gap.rs", ATTR_GAP_RS),
        ("crates/demo/tests/spawn.rs", INTEGRATION_RS),
        ("crates/demo/src/tests.rs", TESTS_RS),
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
    let (file_line, file_text) = at(INTEGRATION_RS, "INTEGRATION_FILE");
    assert_eq!(
        hits(&out),
        vec![
            format!("crates/demo/src/lib.rs:{inline_line}: [env] {inline_text}"),
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
