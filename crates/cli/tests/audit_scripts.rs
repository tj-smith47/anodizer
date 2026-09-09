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
    let (forged_line, forged_text) = at(LIB_RS, "tag-family-ok: fake");
    assert_eq!(
        hits(&out),
        vec![
            format!("crates/demo/src/lib.rs:{line} (fn after_the_inline_module): {text}"),
            format!("crates/demo/src/lib.rs:{forged_line} (fn forged_tag_family): {forged_text}"),
        ],
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
    let (hand_line, hand_text) = at(TESTS_RS, "SIBLING_JUSTIFIED");
    let (named_line, named_text) = at(NAMED_TESTS_RS, "NAMED_SIBLING_FILE");
    let (file_line, file_text) = at(INTEGRATION_RS, "INTEGRATION_FILE");
    let (env_forge_line, env_forge_text) = at(TESTS_RS, "env-ok: fake");
    let (cwd_forge_line, cwd_forge_text) = at(TESTS_RS, "cwd-ok: fake");
    assert_eq!(
        hits(&out),
        vec![
            format!("crates/demo/src/lib.rs:{inline_line}: [env] {inline_text}"),
            format!("crates/demo/src/named_tests.rs:{named_line}: [env] {named_text}"),
            format!("crates/demo/src/tests.rs:{hand_line}: [env-guard] {hand_text}"),
            format!("crates/demo/src/tests.rs:{env_forge_line}: [env] {env_forge_text}"),
            format!("crates/demo/src/tests.rs:{cwd_forge_line}: [cwd] {cwd_forge_text}"),
            format!("crates/demo/src/tests.rs:{sibling_line}: [env] {sibling_text}"),
            format!("crates/demo/tests/spawn.rs:{file_line}: [env] {file_text}"),
        ],
        "{out}"
    );
    assert_eq!(code, 1, "{out}");
}

/// A marker justifies the RACE; it does not justify a restore written out by
/// hand, which a failing assertion between the two halves skips outright,
/// leaking the override into the next test in the process. So a marked
/// mutation is reported `[env-guard]` unless its function names `EnvGuard` —
/// the fixture's `sibling_env_guard`, whose raw call IS the guard's own body.
/// The distinction is what makes the class rule enforceable: it flags the
/// hand-paired shape without flagging the guard that replaces it.
#[test]
fn test_isolation_audit_demands_a_guard_behind_every_marked_env_mutation() {
    let dir = fixture_tree();
    let (_, out) = run_audit("audit-test-isolation.sh", dir.path());

    let (hand_line, _) = at(TESTS_RS, "SIBLING_JUSTIFIED");
    let (guarded_line, _) = at(TESTS_RS, "SIBLING_GUARDED");
    let reported: Vec<String> = hits(&out)
        .into_iter()
        .filter(|h| h.contains("[env-guard]"))
        .collect();
    assert_eq!(
        reported,
        vec![format!(
            "crates/demo/src/tests.rs:{hand_line}: [env-guard] unsafe {{ std::env::set_var(\"SIBLING_JUSTIFIED\", \"1\") }}; // env-ok: serialised by serial(sibling_env)"
        )],
        "only the hand-restored call is reported; line {guarded_line} binds a guard.\n{out}"
    );
}

#[test]
fn spawn_retry_audit_reports_test_context_only() {
    let dir = fixture_tree();
    let (code, out) = run_audit("audit-test-spawn-retry.sh", dir.path());

    let (inline_line, inline_text) = at(LIB_RS, r#"arg("status")"#);
    let (file_line, file_text) = at(INTEGRATION_RS, r#"arg("init")"#);
    let (forged_line, forged_text) = at(TESTS_RS, "spawn-retry-ok: fake");
    assert_eq!(
        hits(&out),
        vec![
            format!("crates/demo/src/lib.rs:{inline_line}: {inline_text}"),
            format!("crates/demo/src/tests.rs:{forged_line}: {forged_text}"),
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
    let (forged_line, forged_text) = at(TESTS_RS, "exec-writer-ok: fake");
    assert_eq!(
        hits(&out),
        vec![
            format!("crates/demo/src/lib.rs:{set_mode_line}: {set_mode_text}"),
            format!("crates/demo/src/tests.rs:{from_mode_line}: {from_mode_text}"),
            format!("crates/demo/src/tests.rs:{builder_line}: {builder_text}"),
            format!("crates/demo/src/tests.rs:{forged_line}: {forged_text}"),
        ],
        "{out}"
    );
    assert_eq!(code, 1, "{out}");
}

/// The two audits whose whole surface is a marker: `audit-log-status.sh`'s
/// `status-ok:` and `audit-repo-identity.sh`'s `slug-ok:` / `token-ok:`. Each
/// fixture pairs one genuine marker with one spelled inside a string literal;
/// only the forged one is a hit, because a marker is read from the comment
/// half of its line and a string literal is code.
#[test]
fn log_status_audit_reads_its_marker_from_the_comment_half() {
    let dir = fixture_tree();
    let (code, out) = run_audit("audit-log-status.sh", dir.path());

    let (forged_line, forged_text) = at(LIB_RS, "status-ok: fake");
    assert_eq!(
        hits(&out),
        vec![format!(
            "crates/demo/src/lib.rs:{forged_line}: {forged_text}"
        )],
        "{out}"
    );
    assert_eq!(code, 1, "{out}");
}

#[test]
fn repo_identity_audit_reads_its_markers_from_the_comment_half() {
    let dir = fixture_tree();
    let (code, out) = run_audit("audit-repo-identity.sh", dir.path());

    let (slug_line, slug_text) = at(LIB_RS, "slug-ok: fake");
    let (token_line, token_text) = at(LIB_RS, "token-ok: fake");
    assert_eq!(
        hits(&out),
        vec![
            format!("crates/demo/src/lib.rs:{slug_line}:    {slug_text}"),
            format!("crates/demo/src/lib.rs:{token_line}:    {token_text}"),
        ],
        "{out}"
    );
    assert_eq!(code, 1, "{out}");
}

/// Every audit scanner runs on bash >= 4.4 — `mapfile` arrived in 4.0 and 4.4
/// is where `set -u` stopped treating an empty array's `"${arr[@]}"` as an
/// unset expansion — and the floor is a property of the script SET, not of a
/// feature list: a scanner that uses no array today grows one tomorrow, and a
/// detection rule keyed on today's spellings (`mapfile `, `[@]}"`, `readarray
/// -t` with index-only expansion) silently stops covering it. So every
/// `audit-*.sh` sources `lib/require-bash.sh`, and no script restates the
/// check inline.
#[test]
fn every_audit_script_sources_the_bash_floor() {
    let mut walked = 0usize;
    let mut missing = Vec::new();
    let mut restated = Vec::new();
    for script in audit_scripts() {
        let name = script
            .file_name()
            .expect("file name")
            .to_string_lossy()
            .into_owned();
        let body = std::fs::read_to_string(&script).expect("script body");
        walked += 1;
        if !body.contains("source \"$LIB_DIR/require-bash.sh\"") {
            missing.push(name.clone());
        }
        if body.contains("BASH_VERSINFO") || body.contains("bash --version") {
            restated.push(name);
        }
    }
    assert!(
        missing.is_empty(),
        "every audit script sources lib/require-bash.sh; these do not: {missing:?}"
    );
    assert!(
        restated.is_empty(),
        "the bash floor is asserted once, in lib/require-bash.sh; these restate it: {restated:?}"
    );
    assert!(
        walked >= 14,
        "expected every audit script to be walked, found {walked}"
    );
}

/// A scanner that could not run must never read as a clean scan. Every audit
/// script that loads an awk library from `.claude/scripts/lib` is driven from
/// a copy whose sibling `lib/` is empty, so awk dies loading its source: the
/// script has to fail with that error visible instead of printing an empty hit
/// list and exiting 0.
#[test]
fn a_scanner_that_cannot_load_its_awk_library_fails_loudly() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = TempDir::new().expect("temp dir");
    let lib = dir.path().join("lib");
    std::fs::create_dir(&lib).expect("lib dir");
    // Only the awk sources are withheld: the shared bash libraries still have
    // to load, or the scripts would die before ever reaching a scanner.
    for shared in ["require-bash.sh", "scan.sh"] {
        std::fs::copy(
            repo.join(".claude/scripts/lib").join(shared),
            lib.join(shared),
        )
        .unwrap_or_else(|e| panic!("copy lib/{shared}: {e}"));
    }

    let mut checked = 0usize;
    for entry in std::fs::read_dir(repo.join(".claude/scripts")).expect("scripts dir") {
        let src = entry.expect("script entry").path();
        let name = src
            .file_name()
            .expect("file name")
            .to_string_lossy()
            .into_owned();
        if !name.starts_with("audit-") || !name.ends_with(".sh") {
            continue;
        }
        // `-f "$LIB_DIR/…"` is an awk source load; `source "$LIB_DIR/…"` is
        // the bash floor, which every script takes and no scanner depends on.
        if !std::fs::read_to_string(&src)
            .expect("script body")
            .contains("-f \"$LIB_DIR/")
        {
            continue;
        }
        checked += 1;
        let copy = dir.path().join(&name);
        std::fs::copy(&src, &copy).expect("copy the script beside an empty lib dir");
        let out = Command::new("bash")
            .arg(&copy)
            .arg(&repo)
            .output()
            .unwrap_or_else(|e| panic!("running {name}: {e}"));
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{name} must exit 2 (\"the scan did not run\"), never 0 or the \
             violations-found 1.\n{stdout}{stderr}"
        );
        assert!(
            stderr.contains(".awk"),
            "{name} must leave the awk error visible, got: {stderr}"
        );
        assert!(
            stderr.contains("the scan did not run"),
            "{name} must say the scan did not run, got: {stderr}"
        );
    }
    assert!(
        checked >= 8,
        "expected every awk-library scanner to be driven, found {checked}"
    );
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

/// The `cmds:` body of one top-level Taskfile target: from its key line to the
/// next top-level key, mirroring `task_block` in `audit-gate-mirror.sh`.
fn taskfile_block(taskfile: &str, target: &str) -> String {
    let key = format!("  {target}:");
    let mut lines = taskfile.lines().skip_while(|l| *l != key).peekable();
    let first = lines
        .next()
        .unwrap_or_else(|| panic!("no `{key}` in Taskfile.yml"));
    let mut block = String::from(first);
    for line in lines {
        let top_level_key = line.starts_with("  ")
            && !line.starts_with("   ")
            && line.ends_with(':')
            && line[2..3]
                .chars()
                .all(|c| c.is_ascii_alphabetic() || c == '_');
        if top_level_key {
            break;
        }
        block.push('\n');
        block.push_str(line);
    }
    block
}

/// Whether a Taskfile `cmds:` block runs `target` as a subtask. Anchored on
/// the whole target name so `docs:validate-readme` never reads as `doc`.
fn runs_task(block: &str, target: &str) -> bool {
    block.lines().any(|line| {
        line.trim()
            .strip_prefix("- task: ")
            .and_then(|rest| rest.strip_prefix(target))
            .is_some_and(|tail| tail.is_empty() || tail.starts_with(char::is_whitespace))
    })
}

/// Rustdoc over this workspace holds several GB, so it may not run on the
/// commit path; it is CI's job and `task gate`'s (and so `task push`'s). The
/// wiring spans four files that no compiler ties together — a rename or a
/// dropped line on one side leaves the gate silently ungated — so pin all of
/// them textually, with no dependency on a `task` binary being installed.
#[test]
fn rustdoc_gate_is_wired_into_gate_and_ci_never_commit() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let taskfile = std::fs::read_to_string(repo.join("Taskfile.yml")).expect("Taskfile.yml");

    let doc = taskfile_block(&taskfile, "doc");
    assert!(
        doc.contains("_check:mem-headroom"),
        "the `doc:` target must refuse to start without memory headroom, got:\n{doc}"
    );
    assert!(
        doc.contains("cargo doc --workspace --no-deps --document-private-items"),
        "the `doc:` target must run the workspace rustdoc, got:\n{doc}"
    );

    let gate = taskfile_block(&taskfile, "gate");
    assert!(
        runs_task(&gate, "doc"),
        "`task gate` must run the rustdoc gate, got:\n{gate}"
    );
    for commit_path in ["audit:code", "lint"] {
        let block = taskfile_block(&taskfile, commit_path);
        assert!(
            !runs_task(&block, "doc"),
            "`task {commit_path}` runs on every commit and must not chain the rustdoc gate, got:\n{block}"
        );
    }

    let ci = std::fs::read_to_string(repo.join(".github/workflows/ci.yml")).expect("ci.yml");
    assert!(
        ci.contains("\n  rustdoc:\n"),
        "ci.yml must carry a `rustdoc` job"
    );
    assert!(
        ci.contains("run: task doc"),
        "ci.yml's rustdoc job must run `task doc`"
    );

    let mirror = std::fs::read_to_string(repo.join(".claude/scripts/audit-gate-mirror.sh"))
        .expect("audit-gate-mirror.sh");
    assert!(
        mirror.contains(r#"[rustdoc]="doc""#),
        "the gate mirror must map ci.yml's rustdoc job to the local `doc` target"
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

/// Every `audit-*.sh` under `.claude/scripts`.
fn audit_scripts() -> Vec<std::path::PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(".claude/scripts");
    let mut found: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .expect("scripts dir")
        .map(|e| e.expect("script entry").path())
        .filter(|p| {
            let name = p.file_name().unwrap_or_default().to_string_lossy();
            name.starts_with("audit-") && name.ends_with(".sh")
        })
        .collect();
    found.sort();
    found
}

/// Whether `line` starts a command whose program is `awk`. A command starts at
/// the line, or after one of the shell's command separators, so
/// `x="$(awk …)"`, `… | awk …` and a bare `awk …` all count while the word
/// inside a comment or a `.awk` path does not.
fn invokes_awk(line: &str) -> bool {
    if line.trim_start().starts_with('#') {
        return false;
    }
    line.split(['|', ';', '&', '(', ')', '`', '{', '}'])
        .map(str::trim_start)
        .any(|cmd| {
            cmd.strip_prefix("awk")
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
        })
}

/// A scanner that could not run must not read as a clean scan, and the thing
/// that decides so is `lib/scan.sh`'s `run_scanner`: it captures awk's stdout,
/// leaves awk's stderr visible and exits 2 on any non-zero awk status. A bare
/// `var="$(awk …)"` under `set -e` instead exits 1 — the repo's
/// "violations found" code — with an empty findings block, so a syntax error
/// in the program reads as a passing audit. Textual rather than behavioural so
/// a script added tomorrow with the old shape is caught before it ever runs.
#[test]
fn every_awk_invocation_goes_through_the_shared_runner() {
    let mut bare = Vec::new();
    let mut scanners = 0usize;
    for script in audit_scripts() {
        let name = script.file_name().expect("file name").to_string_lossy();
        let body = std::fs::read_to_string(&script).expect("script body");
        for (index, line) in body.lines().enumerate() {
            if invokes_awk(line) {
                bare.push(format!("{name}:{}: {}", index + 1, line.trim()));
            }
        }
        if body.contains("run_scanner ") {
            scanners += 1;
            assert!(
                body.contains("source \"$LIB_DIR/scan.sh\""),
                "{name} calls run_scanner without sourcing lib/scan.sh"
            );
        }
    }
    assert!(
        bare.is_empty(),
        "every awk program runs through lib/scan.sh's run_scanner; these invoke awk directly: {bare:#?}"
    );
    assert!(
        scanners >= 8,
        "expected every awk-running scanner to be walked, found {scanners}"
    );
}

/// The companion to `a_scanner_that_cannot_load_its_awk_library_fails_loudly`
/// for a scanner whose program is INLINE: it loads no `lib/*.awk`, so an empty
/// library directory proves nothing about it. Break the program itself instead
/// — the same failure a bad dynamic regex or a mistyped function produces.
#[test]
fn a_scanner_whose_inline_program_is_broken_fails_loudly() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let scripts = repo.join(".claude/scripts");
    let dir = TempDir::new().expect("temp dir");
    let lib = dir.path().join("lib");
    std::fs::create_dir(&lib).expect("lib dir");
    for entry in std::fs::read_dir(scripts.join("lib")).expect("lib dir") {
        let src = entry.expect("lib entry").path();
        std::fs::copy(&src, lib.join(src.file_name().expect("file name"))).expect("copy lib file");
    }

    const SCRIPT: &str = "audit-log-status.sh";
    let body = std::fs::read_to_string(scripts.join(SCRIPT)).expect("script body");
    let broken = body.replacen("<<'AWK'\n", "<<'AWK'\n(((\n", 1);
    assert_ne!(
        broken, body,
        "{SCRIPT} no longer opens its program with <<'AWK'"
    );
    let copy = dir.path().join(SCRIPT);
    std::fs::write(&copy, broken).expect("write the broken copy");

    let out = Command::new("bash")
        .arg(&copy)
        .arg(&repo)
        .output()
        .expect("running the broken scanner");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(2),
        "a syntax error in the program must exit 2, not the violations-found 1.\n{stdout}{stderr}"
    );
    assert!(
        stderr.contains("audit-log-status: awk scanner exited")
            && stderr.contains("the scan did not run"),
        "the runner must name the script and say the scan did not run, got: {stderr}"
    );
    assert!(
        stderr.contains("awk:"),
        "awk's own diagnostic must stay visible, got: {stderr}"
    );
}
