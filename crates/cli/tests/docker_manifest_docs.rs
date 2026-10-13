//! The lines the docker manifests page quotes for a manifest with no images
//! are the lines a release prints for the page's own config block.

mod common;
use common::*;

use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

const PAGE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/site/content/docs/packages/docker-manifests.md"
);

/// The fenced blocks of the page's "Manifests with no images" section, as
/// `(language, body lines)`.
fn no_images_blocks() -> Vec<(String, Vec<String>)> {
    let page = std::fs::read_to_string(PAGE).unwrap();
    let section: Vec<&str> = page
        .lines()
        .skip_while(|line| *line != "## Manifests with no images")
        .skip(1)
        .take_while(|line| !line.starts_with("## "))
        .collect();
    let mut blocks = Vec::new();
    let mut open: Option<(String, Vec<String>)> = None;
    for line in section {
        match (line.strip_prefix("```"), open.take()) {
            (Some(_), Some(block)) => blocks.push(block),
            (Some(language), None) => open = Some((language.to_string(), Vec::new())),
            (None, Some((language, mut body))) => {
                body.push(line.to_string());
                open = Some((language, body));
            }
            (None, None) => {}
        }
    }
    blocks
}

/// Run a dry-run release in `dir` and return its non-blank output lines, at
/// the columns the renderer prints them.
fn release_lines(dir: &Path, extra: &[&str]) -> Vec<String> {
    let out = Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(["release", "--dry-run", "--skip=build"])
        .args(extra)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{printed}");
    printed
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect()
}

/// Whether `block` appears in `lines` as one unbroken run.
fn holds_run(lines: &[String], block: &[String]) -> bool {
    lines.windows(block.len()).any(|window| window == block)
}

#[test]
fn the_lines_quoted_in_the_docker_manifests_docs_are_what_a_release_prints() {
    // The page is a claim about a real run; a host without the tools the run
    // needs fails here, so the claim is never left unchecked.
    assert!(
        tool_on_path("git") && tool_on_path("cargo"),
        "this test runs a real release and needs git and cargo on PATH"
    );
    let blocks = no_images_blocks();
    let languages: Vec<&str> = blocks.iter().map(|(l, _)| l.as_str()).collect();
    assert_eq!(languages, ["yaml", "text", "text", "text"], "{blocks:?}");

    let tmp = TempDir::new().unwrap();
    bootstrap_minimal_cargo_repo(tmp.path(), "myapp");
    std::fs::write(tmp.path().join(".anodizer.yaml"), blocks[0].1.join("\n")).unwrap();
    run_git(tmp.path(), &["commit", "-q", "-am", "manifest config"]);
    run_git(tmp.path(), &["tag", "myapp-v1.2.3"]);

    let default_run = release_lines(tmp.path(), &[]);
    for (_, block) in &blocks[1..3] {
        assert!(
            holds_run(&default_run, block),
            "{block:?} not in {default_run:?}"
        );
    }
    assert!(
        !holds_run(&default_run, &blocks[3].1),
        "the per-manifest skip line prints only under --show-skipped: {default_run:?}"
    );
    let shown_run = release_lines(tmp.path(), &["--show-skipped"]);
    assert!(
        holds_run(&shown_run, &blocks[3].1),
        "{:?} not in {shown_run:?}",
        blocks[3].1
    );

    // The section's blocks are matched above; this count reads the whole
    // page, so a `Warning ` line quoted in another section fails here until
    // it is held by a test too.
    let page = std::fs::read_to_string(PAGE).unwrap();
    let warnings = page
        .lines()
        .filter(|line| line.trim_start().starts_with("Warning "))
        .count();
    assert_eq!(warnings, 1, "the warnings the page quotes");
}
