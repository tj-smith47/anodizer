//! Every ```yaml block on every docs page and README is a config a reader
//! can copy: it loads through `anodizer check config` as written.
//!
//! Two kinds of block are not a whole config, and each says so on the page
//! (`CONTRIBUTING.md` holds the convention):
//!
//! - a piece of a config shown on its own carries the `fragment` fence tag,
//!   ```` ```yaml,fragment ````;
//! - YAML that is some other file (a workflow, a manifest) opens with the
//!   line `# not an anodizer config`.
//!
//! A marked block that does load as a config fails too, so a mark never
//! hides a block that could be checked.

use anodizer_core::test_helpers::docs_pages::{
    FencedBlock, docs_dir, docs_pages, fenced_blocks, readme_pages, repo_root,
};
use anodizer_core::test_helpers::scripted_responder::{ScriptedRoute, spawn_scripted_responder};
use serde_yaml_ng::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

const NOT_A_CONFIG: &str = "# not an anodizer config";

/// What a page says a yaml block is.
#[derive(Debug, PartialEq)]
enum Kind {
    Config,
    Fragment,
    NotAConfig,
}

fn kind(block: &FencedBlock) -> Result<Kind, String> {
    let fragment = match block.tags().as_slice() {
        [] => false,
        ["fragment"] => true,
        other => return Err(format!("unknown fence tags {other:?}")),
    };
    let not_a_config = block.body.lines().next() == Some(NOT_A_CONFIG);
    match (fragment, not_a_config) {
        (false, false) => Ok(Kind::Config),
        (true, false) => Ok(Kind::Fragment),
        (false, true) => Ok(Kind::NotAConfig),
        (true, true) => Err("carries both the fragment tag and the not-a-config line".into()),
    }
}

/// One yaml block to load, named by its page and the line of its fence.
struct Job {
    at: String,
    kind: Kind,
    body: String,
}

fn yaml_blocks() -> (Vec<Job>, Vec<String>) {
    let mut jobs = Vec::new();
    let mut failures = Vec::new();
    let site = docs_pages().into_iter().map(|page| {
        (
            docs_dir().join(&page),
            format!("docs/site/content/docs/{}", page.display()),
        )
    });
    let readmes = readme_pages()
        .into_iter()
        .map(|page| (repo_root().join(&page), page.display().to_string()));
    for (path, name) in site.chain(readmes) {
        let text = std::fs::read_to_string(&path).unwrap();
        for block in fenced_blocks(&text) {
            if !matches!(block.language(), "yaml" | "yml") {
                continue;
            }
            let at = format!("{name}:{}", block.line);
            match kind(&block) {
                Ok(kind) => jobs.push(Job {
                    at,
                    kind,
                    body: block.body,
                }),
                Err(e) => failures.push(format!("{at}: {e}")),
            }
        }
    }
    (jobs, failures)
}

/// The crates a config names, top-level and in every workspace.
fn crates(config: &Value) -> Vec<&Value> {
    fn list(v: Option<&Value>) -> Vec<&Value> {
        v.and_then(Value::as_sequence)
            .map(|s| s.iter().collect())
            .unwrap_or_default()
    }
    let mut all = list(config.get("crates"));
    for ws in list(config.get("workspaces")) {
        all.extend(list(ws.get("crates")));
    }
    all
}

/// Lay out what the config names on disk under `dir`: every crate
/// directory, every build directory and every local include file. A URL
/// include is pointed at `served`, which answers with an empty config.
/// Returns the config text to load.
fn lay_out(dir: &Path, body: &str, served: &str) -> String {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"myapp\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
    let Ok(config) = serde_yaml_ng::from_str::<Value>(body) else {
        return body.to_string();
    };
    let mkdir = |p: Option<&Value>| {
        if let Some(p) = p
            .and_then(Value::as_str)
            .filter(|p| !Path::new(p).is_absolute())
        {
            std::fs::create_dir_all(dir.join(p)).unwrap();
        }
    };
    for krate in crates(&config) {
        mkdir(krate.get("path"));
        for build in krate
            .get("builds")
            .and_then(Value::as_sequence)
            .into_iter()
            .flatten()
        {
            mkdir(build.get("dir"));
        }
    }
    let mut text = body.to_string();
    for include in config
        .get("includes")
        .and_then(Value::as_sequence)
        .into_iter()
        .flatten()
    {
        let file = include
            .as_str()
            .or_else(|| include.get("from_file")?.get("path")?.as_str());
        if let Some(file) = file {
            let file = dir.join(file);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, "{}\n").unwrap();
        }
        if let Some(url) = include
            .get("from_url")
            .and_then(|u| u.get("url"))
            .and_then(Value::as_str)
        {
            text = text.replace(url, served);
        }
    }
    text
}

/// Run `anodizer check config` in `dir`; the error lines it printed when it
/// refused the config.
fn check_config(dir: &Path) -> Option<String> {
    let out = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["check", "config"])
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    if out.status.success() {
        return None;
    }
    // The fixture directory is a temp path that means nothing on the page.
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
    .replace(&format!("{}/", dir.display()), "");
    let errors: Vec<&str> = printed
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("Error") || l.starts_with("caused by"))
        .collect();
    Some(if errors.is_empty() {
        printed.trim().to_string()
    } else {
        errors.join("\n    ")
    })
}

#[test]
fn every_yaml_block_in_the_docs_loads_as_a_config() {
    let (jobs, mut failures) = yaml_blocks();
    let (addr, _) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "GET",
        path_pattern: "/include.yaml",
        response: "HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\n{}\n",
        times: None,
    }]);
    let served = format!("http://{addr}/include.yaml");

    // The fixture is one git repository whose origin names a GitHub
    // repository, so a release block that leaves owner and name to the
    // remote resolves them; each block gets its own directory inside it.
    let tmp = tempfile::TempDir::new().unwrap();
    for args in [
        &["init", "-q"][..],
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/myorg/myapp.git",
        ],
    ] {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.current_dir(tmp.path()).args(args);
                cmd
            },
            "git",
        );
        assert!(out.status.success(), "git {args:?}");
    }

    let dirs: Vec<PathBuf> = (0..jobs.len())
        .map(|i| tmp.path().join(format!("b{i}")))
        .collect();
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
    let results: Vec<Option<String>> = std::thread::scope(|scope| {
        let chunk = jobs.len().div_ceil(threads);
        let handles: Vec<_> = jobs
            .chunks(chunk)
            .zip(dirs.chunks(chunk))
            .map(|(jobs, dirs)| {
                let served = &served;
                scope.spawn(move || {
                    jobs.iter()
                        .zip(dirs)
                        .map(|(job, dir)| {
                            let text = lay_out(dir, &job.body, served);
                            std::fs::write(dir.join(".anodizer.yaml"), text).unwrap();
                            check_config(dir)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });

    let mut loaded = 0usize;
    for (job, refused) in jobs.iter().zip(&results) {
        match (&job.kind, refused) {
            (Kind::Config, None) => loaded += 1,
            (Kind::Config, Some(e)) => {
                failures.push(format!("{}: does not load:\n    {e}", job.at))
            }
            (Kind::Fragment, None) => failures.push(format!(
                "{}: loads as a whole config, so the `fragment` tag is wrong; remove it",
                job.at
            )),
            (Kind::NotAConfig, None) => failures.push(format!(
                "{}: loads as a whole config, so the `{NOT_A_CONFIG}` line is wrong; remove it",
                job.at
            )),
            (_, Some(_)) => {}
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    println!("{loaded} yaml blocks loaded");
    assert!(loaded > 400, "the docs hold their config blocks: {loaded}");
}
