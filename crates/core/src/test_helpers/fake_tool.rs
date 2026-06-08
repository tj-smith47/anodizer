//! PATH-stub harness for faking external command-line tools in tests.
//!
//! Many stages shell out to external tools — `codesign`/`xcrun` (notarize),
//! `syft` (sbom), `makeself`, `rpmbuild` (srpm), `docker`, `upx`, `appimagetool`.
//! Those code paths are the largest uncovered blocks in the tree precisely
//! because a real run needs the tool installed, often on a specific OS. This
//! harness makes them testable on any host by installing executable stub
//! scripts that emit canned stdout/stderr, exit with a chosen code, optionally
//! create output files, and record every invocation's argv for assertions.
//!
//! Two ways to route a stage at a stub:
//!
//! 1. **Configurable command** (e.g. sbom's `cmd:`): point the config value
//!    straight at [`FakeToolDir::tool_path`] — no PATH mutation, no `#[serial]`.
//! 2. **Hard-coded tool name** (e.g. notarize's `codesign`): call
//!    [`FakeToolDir::activate`] to prepend the stub dir to `PATH`. This mutates
//!    the process environment, so such tests **must** be `#[serial]` (the guard
//!    holds [`crate::test_helpers::env::env_mutex`] for its lifetime and
//!    restores the prior `PATH` on drop).
//!
//! ## Example — configurable command (sbom)
//!
//! ```no_run
//! use anodizer_core::test_helpers::fake_tool::FakeToolDir;
//!
//! let tools = FakeToolDir::new();
//! tools
//!     .tool("syft")
//!     .creates("sbom.spdx.json", "{}")
//!     .stdout("generated 1 document\n")
//!     .install();
//! // point the sbom `cmd:` config at tools.tool_path("syft") ...
//! // run the stage ...
//! assert!(tools.was_called("syft"));
//! let argv = tools.calls("syft");
//! assert_eq!(argv[0][0], "scan"); // first arg of the first invocation
//! ```
//!
//! ## Example — hard-coded tool on PATH (notarize), serialised
//!
//! ```no_run
//! use anodizer_core::test_helpers::fake_tool::FakeToolDir;
//!
//! // #[test] #[serial] fn notarize_happy_path() {
//! let tools = FakeToolDir::new();
//! tools.tool("codesign").install();
//! tools.tool("xcrun").stdout("{\"status\":\"Accepted\"}\n").install();
//! let _path = tools.activate(); // prepend to PATH until `_path` drops
//! // run the notarize stage ...
//! // }
//! ```

use std::path::{Path, PathBuf};

use tempfile::TempDir;

use crate::test_helpers::env::env_mutex;

/// Argument separator written by stub scripts between successive argv entries.
const ARG_SEP: char = '\u{1f}';
/// Record separator written by stub scripts after each invocation's argv.
const REC_SEP: char = '\u{1e}';

/// A temporary directory of installed fake-tool stubs.
///
/// Lives for the duration of a test; the backing temp dir (and every stub in
/// it) is deleted when this value drops. Build stubs with [`tool`](Self::tool),
/// route stages at them via [`tool_path`](Self::tool_path) or
/// [`activate`](Self::activate), then assert with [`was_called`](Self::was_called)
/// / [`calls`](Self::calls).
pub struct FakeToolDir {
    dir: TempDir,
}

impl FakeToolDir {
    /// Create a fresh, empty stub directory backed by a temp dir.
    ///
    /// # Panics
    /// Panics if the temp directory cannot be created.
    pub fn new() -> Self {
        let dir = TempDir::new().expect("fake_tool: create temp dir");
        Self { dir }
    }

    /// The directory holding the installed stubs (what [`activate`](Self::activate)
    /// prepends to `PATH`).
    pub fn bin_dir(&self) -> &Path {
        self.dir.path()
    }

    /// Absolute path to a named stub, suitable for a configurable `cmd:` field.
    /// The stub need not exist yet; pair with [`tool`](Self::tool)`.install()`.
    pub fn tool_path(&self, name: &str) -> PathBuf {
        self.dir.path().join(stub_file_name(name))
    }

    /// Begin defining a stub tool. Chain setters, then call
    /// [`ToolSpec::install`].
    pub fn tool<'a>(&'a self, name: &str) -> ToolSpec<'a> {
        ToolSpec {
            dir: self,
            name: name.to_string(),
            stdout: String::new(),
            stderr: String::new(),
            exit: 0,
            script: None,
            creates: Vec::new(),
        }
    }

    /// Prepend [`bin_dir`](Self::bin_dir) to `PATH` for hard-coded tool names.
    ///
    /// Returns a guard that restores the prior `PATH` and releases the env mutex
    /// when dropped. The test **must** be `#[serial]`; the guard holds the
    /// shared [`env_mutex`] so it cannot race other env-mutating tests, but
    /// `#[serial]` is still required because coverage runs share one process.
    pub fn activate(&self) -> PathGuard {
        let lock = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var_os("PATH");
        let mut entries: Vec<PathBuf> = vec![self.dir.path().to_path_buf()];
        if let Some(ref p) = prior {
            entries.extend(std::env::split_paths(p));
        }
        let joined = std::env::join_paths(entries).expect("fake_tool: join PATH");
        // SAFETY: serialised by the env mutex held in `lock` for the guard's life.
        unsafe { std::env::set_var("PATH", &joined) };
        PathGuard { prior, _lock: lock }
    }

    /// Every recorded invocation of `name`, outer `Vec` per call, inner `Vec`
    /// the argv (excluding arg0). Empty if the stub was never run.
    pub fn calls(&self, name: &str) -> Vec<Vec<String>> {
        let log = self.calls_path(name);
        let Ok(raw) = std::fs::read_to_string(&log) else {
            return Vec::new();
        };
        raw.split(REC_SEP)
            .filter(|rec| !rec.is_empty())
            .map(|rec| {
                rec.split(ARG_SEP)
                    .filter(|a| !a.is_empty())
                    .map(|a| a.to_string())
                    .collect()
            })
            .collect()
    }

    /// Whether `name` was invoked at least once.
    pub fn was_called(&self, name: &str) -> bool {
        self.calls_path(name).exists()
    }

    /// Number of times `name` was invoked.
    pub fn call_count(&self, name: &str) -> usize {
        self.calls(name).len()
    }

    fn calls_path(&self, name: &str) -> PathBuf {
        self.dir.path().join(format!(".calls.{name}"))
    }
}

impl Default for FakeToolDir {
    fn default() -> Self {
        Self::new()
    }
}

/// Builder for a single fake tool. Created by [`FakeToolDir::tool`].
#[must_use = "call `.install()` to write the stub"]
pub struct ToolSpec<'a> {
    dir: &'a FakeToolDir,
    name: String,
    stdout: String,
    stderr: String,
    exit: i32,
    script: Option<String>,
    creates: Vec<(String, String)>,
}

impl ToolSpec<'_> {
    /// Text the stub writes to stdout.
    pub fn stdout(mut self, s: impl Into<String>) -> Self {
        self.stdout = s.into();
        self
    }

    /// Text the stub writes to stderr.
    pub fn stderr(mut self, s: impl Into<String>) -> Self {
        self.stderr = s.into();
        self
    }

    /// Exit code the stub returns (default `0`).
    pub fn exit(mut self, code: i32) -> Self {
        self.exit = code;
        self
    }

    /// Make the stub create an output file (path relative to the tool's working
    /// directory, or absolute) with the given contents before exiting. Useful
    /// for tools whose stage asserts the artifact exists (sbom, makeself, srpm).
    /// Unix only.
    pub fn creates(mut self, rel_path: impl Into<String>, contents: impl Into<String>) -> Self {
        self.creates.push((rel_path.into(), contents.into()));
        self
    }

    /// Replace the default emit-and-exit body with an arbitrary `sh` snippet,
    /// run after argv is recorded. The snippet can read `"$@"` and create files.
    /// Unix only; overrides [`stdout`](Self::stdout)/[`stderr`](Self::stderr)/
    /// [`exit`](Self::exit).
    pub fn script(mut self, body: impl Into<String>) -> Self {
        self.script = Some(body.into());
        self
    }

    /// Write the stub to disk and make it executable.
    ///
    /// # Panics
    /// Panics if the stub cannot be written or marked executable.
    pub fn install(self) {
        let path = self.dir.tool_path(&self.name);
        let calls = self.dir.calls_path(&self.name);
        let body = self.render_script(&calls);
        std::fs::write(&path, body).expect("fake_tool: write stub");
        make_executable(&path);
    }

    #[cfg(unix)]
    fn render_script(&self, calls: &Path) -> String {
        let mut s = String::from("#!/bin/sh\n");
        // Record argv (excluding arg0) as ARG_SEP-joined, REC_SEP-terminated.
        s.push_str(&format!(
            "{{ printf '%s{arg}' \"$@\"; printf '{rec}'; }} >> {log}\n",
            arg = ARG_SEP,
            rec = REC_SEP,
            log = sh_quote(&calls.to_string_lossy()),
        ));
        for (rel, contents) in &self.creates {
            // mkdir -p the parent so relative nested outputs work.
            s.push_str(&format!(
                "mkdir -p \"$(dirname {p})\" 2>/dev/null || true\n",
                p = sh_quote(rel)
            ));
            s.push_str(&format!(
                "printf '%s' {c} > {p}\n",
                c = sh_quote(contents),
                p = sh_quote(rel),
            ));
        }
        if let Some(custom) = &self.script {
            s.push_str(custom);
            if !custom.ends_with('\n') {
                s.push('\n');
            }
        } else {
            if !self.stdout.is_empty() {
                s.push_str(&format!("printf '%s' {}\n", sh_quote(&self.stdout)));
            }
            if !self.stderr.is_empty() {
                s.push_str(&format!("printf '%s' {} 1>&2\n", sh_quote(&self.stderr)));
            }
            s.push_str(&format!("exit {}\n", self.exit));
        }
        s
    }

    #[cfg(not(unix))]
    fn render_script(&self, calls: &Path) -> String {
        assert!(
            self.script.is_none() && self.creates.is_empty(),
            "fake_tool: .script()/.creates() are unix-only"
        );
        let mut s = String::from("@echo off\r\n");
        s.push_str(&format!(">>\"{}\" echo %*\r\n", calls.display()));
        if !self.stdout.is_empty() {
            s.push_str(&format!(
                "echo|set /p=\"{}\"\r\n",
                self.stdout.replace('\n', " ")
            ));
        }
        if !self.stderr.is_empty() {
            s.push_str(&format!(
                "echo|set /p=\"{}\" 1>&2\r\n",
                self.stderr.replace('\n', " ")
            ));
        }
        s.push_str(&format!("exit /b {}\r\n", self.exit));
        s
    }
}

/// Restores `PATH` and releases the env mutex when dropped. Returned by
/// [`FakeToolDir::activate`].
pub struct PathGuard {
    prior: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        // SAFETY: still serialised by `_lock`, dropped after this.
        unsafe {
            match &self.prior {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
        }
    }
}

#[cfg(unix)]
fn stub_file_name(name: &str) -> String {
    name.to_string()
}

#[cfg(not(unix))]
fn stub_file_name(name: &str) -> String {
    format!("{name}.cmd")
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .expect("fake_tool: stat stub")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).expect("fake_tool: chmod stub");
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

/// Single-quote a string for safe interpolation into an `sh` script.
#[cfg(unix)]
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[cfg(unix)]
    #[test]
    fn records_argv_across_invocations() {
        let tools = FakeToolDir::new();
        tools.tool("widget").stdout("ok\n").install();
        let bin = tools.tool_path("widget");

        let out = Command::new(&bin)
            .args(["build", "--fast"])
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout), "ok\n");
        Command::new(&bin).arg("clean").output().unwrap();

        assert!(tools.was_called("widget"));
        assert_eq!(tools.call_count("widget"), 2);
        let calls = tools.calls("widget");
        assert_eq!(calls[0], vec!["build", "--fast"]);
        assert_eq!(calls[1], vec!["clean"]);
    }

    #[cfg(unix)]
    #[test]
    fn honors_exit_code_and_stderr() {
        let tools = FakeToolDir::new();
        tools.tool("boom").stderr("fatal\n").exit(7).install();
        let out = Command::new(tools.tool_path("boom")).output().unwrap();
        assert_eq!(out.status.code(), Some(7));
        assert_eq!(String::from_utf8_lossy(&out.stderr), "fatal\n");
    }

    #[cfg(unix)]
    #[test]
    fn creates_output_file() {
        let tools = FakeToolDir::new();
        tools
            .tool("gen")
            .creates("out/doc.json", "{\"k\":1}")
            .install();
        let work = TempDir::new().unwrap();
        let out = Command::new(tools.tool_path("gen"))
            .current_dir(work.path())
            .output()
            .unwrap();
        assert!(out.status.success());
        let body = std::fs::read_to_string(work.path().join("out/doc.json")).unwrap();
        assert_eq!(body, "{\"k\":1}");
    }

    #[cfg(unix)]
    #[test]
    fn custom_script_sees_argv() {
        let tools = FakeToolDir::new();
        // syft-style: write the file named after the `-o fmt=PATH` arg.
        tools
            .tool("syft")
            .script("for a in \"$@\"; do case \"$a\" in *=*) echo '{}' > \"${a#*=}\";; esac; done")
            .install();
        let work = TempDir::new().unwrap();
        Command::new(tools.tool_path("syft"))
            .current_dir(work.path())
            .args(["scan", "-o", "spdx-json=bom.json"])
            .output()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(work.path().join("bom.json")).unwrap(),
            "{}\n"
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn activate_prepends_path_and_restores() {
        let before = std::env::var_os("PATH");
        let tools = FakeToolDir::new();
        tools.tool("findme").install();
        {
            let _g = tools.activate();
            let resolved = which_on_path("findme");
            assert_eq!(
                resolved.as_deref(),
                Some(tools.tool_path("findme").as_path())
            );
        }
        assert_eq!(std::env::var_os("PATH"), before);
    }

    #[cfg(unix)]
    fn which_on_path(name: &str) -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|d| d.join(name))
            .find(|p| p.exists())
    }
}
