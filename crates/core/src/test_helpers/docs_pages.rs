//! The documentation pages and the fenced blocks on them, for tests that
//! hold a page to the code.
//!
//! Every test that reads a docs page, or the blocks on one, goes through
//! this module: one listing of the pages and one fence reader, so a page
//! added under `docs/site/content/docs/` or a fence written a new way is
//! seen the same by every test.

use std::path::{Path, PathBuf};

/// The repository root, two levels above `crates/core`.
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root")
}

/// `docs/site/content/docs`, the root of the docs site's pages.
pub fn docs_dir() -> PathBuf {
    repo_root().join("docs/site/content/docs")
}

/// The text of one docs page, named relative to [`docs_dir`]
/// (`"general/templates.md"`).
pub fn read_docs_page(relative: &str) -> String {
    let path = docs_dir().join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Every `.md` file under `dir`, skipping the directory `skip`, as paths
/// relative to `root`.
fn collect_pages(dir: &Path, root: &Path, skip: Option<&Path>, pages: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
        let path = entry.expect("docs entry").path();
        if path.is_dir() {
            if skip != Some(path.as_path()) {
                collect_pages(&path, root, skip, pages);
            }
        } else if path.extension().is_some_and(|ext| ext == "md") {
            pages.push(
                path.strip_prefix(root)
                    .expect("under the root")
                    .to_path_buf(),
            );
        }
    }
}

/// Every `.md` page under [`docs_dir`], as paths relative to it, sorted.
pub fn docs_pages() -> Vec<PathBuf> {
    let root = docs_dir();
    let mut pages = Vec::new();
    collect_pages(&root, &root, None, &mut pages);
    pages.sort();
    assert!(pages.len() > 100, "the docs site has its pages");
    pages
}

/// Every other `.md` page of the docs site: the pages under
/// `docs/site/content` outside [`docs_dir`] (the migration guides, the
/// dogfooding log), as paths relative to [`repo_root`], sorted.
pub fn other_site_pages() -> Vec<PathBuf> {
    let root = repo_root();
    let docs = docs_dir();
    let mut pages = Vec::new();
    collect_pages(
        &root.join("docs/site/content"),
        &root,
        Some(docs.as_path()),
        &mut pages,
    );
    pages.sort();
    assert!(pages.len() > 5, "the docs site has its other pages");
    pages
}

/// The repository README and every workspace crate's README, as paths
/// relative to [`repo_root`], sorted.
pub fn readme_pages() -> Vec<PathBuf> {
    let root = repo_root();
    let mut pages = vec![PathBuf::from("README.md")];
    for krate in super::test_sources::workspace_crate_dirs() {
        let readme = krate.join("README.md");
        if readme.is_file() {
            let krate = krate.canonicalize().expect("crate dir");
            let relative = krate.strip_prefix(&root).expect("crate under the root");
            pages.push(relative.join("README.md"));
        }
    }
    pages
}

/// One fenced code block on a page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FencedBlock {
    /// The 1-based line of the opening fence in the text it was read from.
    pub line: usize,
    /// The info string after the opening fence (`yaml`, `yaml,fragment`).
    pub info: String,
    /// The lines between the fences, each ending in `\n`, with the opening
    /// fence's indent taken off each line, so a block nested under a list
    /// item reads as written at column zero.
    pub body: String,
}

impl FencedBlock {
    /// The language: the info string up to its first comma, which is how
    /// Zola reads it (`yaml,fragment` highlights as `yaml`).
    pub fn language(&self) -> &str {
        self.info.split(',').next().unwrap_or("").trim()
    }

    /// The comma-separated words after the language (`fragment`).
    pub fn tags(&self) -> Vec<&str> {
        self.info.split(',').skip(1).map(str::trim).collect()
    }
}

/// Every fenced block in `page`, in page order.
///
/// A fence is a run of three or more backticks or tildes, indented or not.
/// Only a run of the same character at least as long, with nothing after
/// it, closes the block, so a shorter run inside it is body text.
pub fn fenced_blocks(page: &str) -> Vec<FencedBlock> {
    fn fence(line: &str) -> Option<(usize, char, usize, &str)> {
        let text = line.trim_start();
        let c = text.chars().next().filter(|c| *c == '`' || *c == '~')?;
        let len = text.len() - text.trim_start_matches(c).len();
        (len >= 3).then(|| (line.len() - text.len(), c, len, text[len..].trim()))
    }
    let mut blocks = Vec::new();
    let mut open: Option<(usize, char, usize, FencedBlock)> = None;
    for (at, line) in page.lines().enumerate() {
        match (fence(line), open.take()) {
            (Some((_, c, len, "")), Some((_, oc, olen, block))) if c == oc && len >= olen => {
                blocks.push(block);
            }
            (_, Some((indent, oc, olen, mut block))) => {
                let strip = line.len() - line.trim_start().len();
                block.body.push_str(&line[strip.min(indent)..]);
                block.body.push('\n');
                open = Some((indent, oc, olen, block));
            }
            (Some((indent, c, len, info)), None) => {
                let block = FencedBlock {
                    line: at + 1,
                    info: info.to_string(),
                    body: String::new(),
                };
                open = Some((indent, c, len, block));
            }
            (None, None) => {}
        }
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fenced_blocks_reads_indented_long_and_tilde_fences() {
        let page = "  ```yaml,fragment\n  a: 1\n    b: 2\n  ```\n````text\n```\n````\n~~~dockerfile\nFROM x\n~~~\n";
        let blocks = fenced_blocks(page);
        let read: Vec<(usize, &str, Vec<&str>, &str)> = blocks
            .iter()
            .map(|b| (b.line, b.language(), b.tags(), b.body.as_str()))
            .collect();
        assert_eq!(
            read,
            [
                (1, "yaml", vec!["fragment"], "a: 1\n  b: 2\n"),
                (5, "text", vec![], "```\n"),
                (8, "dockerfile", vec![], "FROM x\n"),
            ]
        );
    }

    /// A test that builds its own path to a docs page reads it past this
    /// module, so a page moved or a fence written a new way is seen by that
    /// test differently from the rest. The marker is a path that climbs out
    /// of a crate to the pages (`../docs/site/content/docs`, every spelling
    /// built on `CARGO_MANIFEST_DIR` or `include_str!`), or the bare page root
    /// on a line that joins or reads it.
    #[test]
    fn every_docs_page_read_goes_through_this_module() {
        const PAGES: &str = "docs/site/content/docs";
        // The docs generator writes the pages rather than reading one for a
        // test, so it finds the page root from its own project root.
        const ALLOWED: [(&str, &str); 1] = [(
            "crates/xtask/src/gen_docs.rs",
            r#"let docs_dir = project_root.join("docs/site/content/docs");"#,
        )];
        let root = repo_root();
        let mut found = Vec::new();
        let mut allowed_hits = [0usize; ALLOWED.len()];
        let sources = super::super::test_sources::workspace_sources();
        for source in &sources {
            let relative = source
                .canonicalize()
                .expect("source path")
                .strip_prefix(&root)
                .expect("a source under the repository root")
                .to_path_buf();
            let relative = crate::path_util::slash_display(&relative);
            if relative == "crates/core/src/test_helpers/docs_pages.rs" {
                continue;
            }
            let text = std::fs::read_to_string(source).expect("readable source");
            for (at, line) in text.lines().enumerate() {
                let code = line.trim_start();
                if code.starts_with("//") || !code.contains(PAGES) {
                    continue;
                }
                let reads = code.contains(&format!("../{PAGES}"))
                    || [
                        "CARGO_MANIFEST_DIR",
                        ".join(",
                        "read_to_string",
                        "include_str!",
                    ]
                    .iter()
                    .any(|needle| code.contains(needle));
                if !reads {
                    continue;
                }
                if let Some(i) = ALLOWED
                    .iter()
                    .position(|(file, allowed)| relative == *file && code == *allowed)
                {
                    allowed_hits[i] += 1;
                    continue;
                }
                found.push(format!("{relative}:{}: {code}", at + 1));
            }
        }
        assert!(sources.len() > 100, "the scan reads the workspace");
        assert!(
            found.is_empty(),
            "read these docs pages through `read_docs_page`:\n{}",
            found.join("\n")
        );
        for ((file, line), hits) in ALLOWED.iter().zip(allowed_hits) {
            assert_eq!(
                hits, 1,
                "the allowed line no longer matches: {file}: {line}"
            );
        }
    }

    #[test]
    fn the_page_listings_find_the_pages() {
        let pages = docs_pages();
        assert!(pages.contains(&PathBuf::from("general/templates.md")));
        let readmes = readme_pages();
        assert!(readmes.contains(&PathBuf::from("README.md")));
        assert!(readmes.contains(&PathBuf::from("crates/cli/README.md")));
    }
}
