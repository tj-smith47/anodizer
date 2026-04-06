# Session I: Archive & Source Behavioral Gaps — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close 9 behavioral gaps between Anodize's archive/source stages and GoReleaser's reference implementation.

**Architecture:** All archive changes land in `crates/stage-archive/src/lib.rs`. Source changes land in `crates/stage-source/src/lib.rs`. A new `by_kinds_and_crate()` method is added to `ArtifactStore`. The `tar` crate is added to stage-source for programmatic tar header manipulation.

**Tech Stack:** Rust, tar crate, glob crate, tera templates

---

### Task 1: Archive — glob directory preservation (LCP logic)

**Files:**
- Modify: `crates/stage-archive/src/lib.rs:373-408` (resolve_file_specs)
- Modify: `crates/stage-archive/src/lib.rs:977-1010` (extra_entries construction)

GoReleaser's `archivefiles.go` computes the longest common prefix (LCP) of all files matched by a glob when `destination` is set, then stores files as `dst/relative_from_lcp`. Anodize flattens everything to just the filename. Reference: `/opt/repos/goreleaser/internal/archivefiles/archivefiles.go:46-63`.

- [ ] **Step 1: Write failing test for LCP helper**

Add to the `#[cfg(test)] mod tests` block in `crates/stage-archive/src/lib.rs`:

```rust
#[test]
fn test_longest_common_prefix() {
    assert_eq!(longest_common_prefix(&[]), "");
    assert_eq!(
        longest_common_prefix(&["a/b/c".to_string(), "a/b/d".to_string()]),
        "a/b/"
    );
    assert_eq!(
        longest_common_prefix(&["x/y/z.txt".to_string(), "x/y/w.txt".to_string()]),
        "x/y/"
    );
    assert_eq!(
        longest_common_prefix(&["/tmp/docs/README.md".to_string()]),
        "/tmp/docs/README.md"
    );
    // No common prefix
    assert_eq!(
        longest_common_prefix(&["abc".to_string(), "xyz".to_string()]),
        ""
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p anodize-stage-archive test_longest_common_prefix 2>&1 | tail -5`
Expected: compile error — `longest_common_prefix` not defined

- [ ] **Step 3: Implement LCP helpers**

Add above `resolve_file_specs()` (around line 370):

```rust
/// Compute longest common prefix of a slice of strings (byte-level).
/// Matches GoReleaser's `longestCommonPrefix` from archivefiles.go.
fn longest_common_prefix(strs: &[String]) -> String {
    if strs.is_empty() {
        return String::new();
    }
    let mut lcp = strs[0].clone();
    for s in &strs[1..] {
        let common_len = lcp
            .bytes()
            .zip(s.bytes())
            .take_while(|(a, b)| a == b)
            .count();
        lcp.truncate(common_len);
    }
    lcp
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p anodize-stage-archive test_longest_common_prefix 2>&1 | tail -5`
Expected: PASS

- [ ] **Step 5: Write failing test for resolve_file_specs with dst preserving directory structure**

```rust
#[test]
fn test_resolve_file_specs_dst_preserves_directory_structure() {
    let tmp = TempDir::new().unwrap();
    // Create docs/README.md and docs/guide/intro.md
    let docs = tmp.path().join("docs");
    fs::create_dir_all(docs.join("guide")).unwrap();
    fs::write(docs.join("README.md"), b"readme").unwrap();
    fs::write(docs.join("guide").join("intro.md"), b"intro").unwrap();

    let pattern = format!("{}/**/*", docs.display());
    let specs = vec![ArchiveFileSpec::Detailed {
        src: pattern,
        dst: Some("mydocs".to_string()),
        info: None,
        strip_parent: None,
    }];

    let resolved = resolve_file_specs(&specs).unwrap();
    assert_eq!(resolved.len(), 2);

    // Should preserve relative structure under dst:
    // docs/README.md      -> mydocs/README.md
    // docs/guide/intro.md -> mydocs/guide/intro.md
    let dsts: Vec<String> = resolved.iter().map(|r| r.dst.clone().unwrap()).collect();
    assert!(dsts.contains(&"mydocs/README.md".to_string()), "got: {:?}", dsts);
    assert!(dsts.contains(&"mydocs/guide/intro.md".to_string()), "got: {:?}", dsts);
}
```

- [ ] **Step 6: Run test to verify it fails**

Run: `cargo test -p anodize-stage-archive test_resolve_file_specs_dst_preserves 2>&1 | tail -10`
Expected: FAIL — dst is just "mydocs" for both files (no relative path appended)

- [ ] **Step 7: Implement LCP-based destination in resolve_file_specs**

Modify `resolve_file_specs()` — the `Detailed` arm. After resolving paths, when `dst` is `Some` and `strip_parent` is false, compute the LCP of matched paths and build `dst/relative_from_lcp_dir` for each file:

```rust
ArchiveFileSpec::Detailed {
    src,
    dst,
    info,
    strip_parent,
} => {
    let paths = resolve_glob_patterns(std::slice::from_ref(src))?;
    let do_strip = strip_parent.unwrap_or(false);

    // When dst is set and strip_parent is false, compute the
    // longest common prefix directory of all matched paths so
    // that relative directory structure is preserved under dst.
    // Matches GoReleaser's archivefiles.go:46-63.
    let lcp_dir: Option<PathBuf> = if dst.is_some() && !do_strip && paths.len() > 0 {
        let path_strs: Vec<String> = paths
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        let prefix = longest_common_prefix(&path_strs);
        // If the prefix is not a directory, use its parent
        let prefix_path = PathBuf::from(&prefix);
        if prefix_path.is_dir() {
            Some(prefix_path)
        } else {
            prefix_path.parent().map(|p| p.to_path_buf())
        }
    } else {
        None
    };

    for p in paths {
        let resolved_dst = if do_strip {
            // strip_parent: just use dst (or filename)
            dst.clone()
        } else if let (Some(ref d), Some(ref lcp)) = (&dst, &lcp_dir) {
            // Compute relative path from LCP dir, join with dst
            let rel = p.strip_prefix(lcp).unwrap_or(&p);
            let joined = PathBuf::from(d).join(rel);
            Some(joined.to_string_lossy().to_string())
        } else {
            dst.clone()
        };

        results.push(ResolvedExtraFile {
            src: p,
            dst: resolved_dst,
            info: info.clone(),
            strip_parent: do_strip,
        });
    }
}
```

- [ ] **Step 8: Run test to verify it passes**

Run: `cargo test -p anodize-stage-archive test_resolve_file_specs_dst_preserves 2>&1 | tail -5`
Expected: PASS

- [ ] **Step 9: Run all archive tests to check for regressions**

Run: `cargo test -p anodize-stage-archive 2>&1 | tail -20`
Expected: all tests pass

- [ ] **Step 10: Commit**

```bash
git add crates/stage-archive/src/lib.rs
git commit -m "feat(archive): implement LCP-based directory preservation for glob dst

When a file spec has a destination set, compute the longest common
prefix of matched paths and preserve relative directory structure
under the destination, matching GoReleaser's archivefiles.go behavior."
```

---

### Task 2: Archive — duplicate destination detection

**Files:**
- Modify: `crates/stage-archive/src/lib.rs:1012-1013` (after all_entries construction)

GoReleaser's `unique()` (archivefiles.go:92-110) warns when the same destination path appears twice and skips the duplicate. Anodize has no such check.

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn test_duplicate_destination_detection() {
    let tmp = TempDir::new().unwrap();
    let file_a = tmp.path().join("a.txt");
    let file_b = tmp.path().join("b.txt");
    fs::write(&file_a, b"aaa").unwrap();
    fs::write(&file_b, b"bbb").unwrap();

    // Two files mapping to the same archive name
    let entries = vec![
        ArchiveEntry {
            src: file_a.clone(),
            archive_name: PathBuf::from("same.txt"),
            info: None,
        },
        ArchiveEntry {
            src: file_b.clone(),
            archive_name: PathBuf::from("same.txt"),
            info: None,
        },
    ];

    let deduped = deduplicate_entries(entries);
    assert_eq!(deduped.len(), 1);
    assert_eq!(deduped[0].src, file_a);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p anodize-stage-archive test_duplicate_destination 2>&1 | tail -5`
Expected: compile error — `deduplicate_entries` not defined

- [ ] **Step 3: Implement deduplicate_entries**

Add near `compute_archive_name()` (around line 467):

```rust
/// Remove entries with duplicate archive_name paths, keeping the first
/// occurrence and warning about skipped duplicates. Matches GoReleaser's
/// `unique()` function in archivefiles.go.
fn deduplicate_entries(entries: Vec<ArchiveEntry>) -> Vec<ArchiveEntry> {
    let mut seen = HashMap::<PathBuf, PathBuf>::new();
    let mut result = Vec::with_capacity(entries.len());
    for entry in entries {
        if let Some(existing_src) = seen.get(&entry.archive_name) {
            eprintln!(
                "Warning: [archive] file '{}' already exists in archive as '{}' — '{}' will be ignored",
                entry.archive_name.display(),
                existing_src.display(),
                entry.src.display(),
            );
        } else {
            seen.insert(entry.archive_name.clone(), entry.src.clone());
            result.push(entry);
        }
    }
    result
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p anodize-stage-archive test_duplicate_destination 2>&1 | tail -5`
Expected: PASS

- [ ] **Step 5: Wire deduplicate_entries into the archive stage**

In the `run()` method, after constructing `binary_entries` and `extra_entries` (around line 1012), replace the direct chain with deduplication:

Replace:
```rust
let all_entries: Vec<&ArchiveEntry> =
    binary_entries.iter().chain(extra_entries.iter()).collect();
```

With:
```rust
let combined: Vec<ArchiveEntry> = binary_entries
    .into_iter()
    .chain(extra_entries.into_iter())
    .collect();
let deduped = deduplicate_entries(combined);
let all_entries: Vec<&ArchiveEntry> = deduped.iter().collect();
```

Note: this changes `binary_entries` and `extra_entries` from being borrowed later, so also update the `all_src_paths` construction (lines 1017-1021) to use `deduped` instead:

```rust
let all_src_paths: Vec<PathBuf> = deduped.iter().map(|e| e.src.clone()).collect();
let path_refs: Vec<&Path> =
    all_src_paths.iter().map(PathBuf::as_path).collect();
```

- [ ] **Step 6: Run all archive tests**

Run: `cargo test -p anodize-stage-archive 2>&1 | tail -20`
Expected: all tests pass

- [ ] **Step 7: Commit**

```bash
git add crates/stage-archive/src/lib.rs
git commit -m "feat(archive): add duplicate destination path detection

Warn and skip when multiple files map to the same archive path,
matching GoReleaser's unique() function in archivefiles.go."
```

---

### Task 3: Archive — file sorting for reproducibility

**Files:**
- Modify: `crates/stage-archive/src/lib.rs` (after deduplication, before writing)

GoReleaser sorts resolved files by destination path (archivefiles.go:66-68). Anodize does not sort.

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn test_archive_entries_sorted_by_destination() {
    let entries = vec![
        ArchiveEntry {
            src: PathBuf::from("z.txt"),
            archive_name: PathBuf::from("c.txt"),
            info: None,
        },
        ArchiveEntry {
            src: PathBuf::from("a.txt"),
            archive_name: PathBuf::from("a.txt"),
            info: None,
        },
        ArchiveEntry {
            src: PathBuf::from("m.txt"),
            archive_name: PathBuf::from("b.txt"),
            info: None,
        },
    ];

    let sorted = sort_entries(entries);
    let names: Vec<String> = sorted
        .iter()
        .map(|e| e.archive_name.to_string_lossy().to_string())
        .collect();
    assert_eq!(names, vec!["a.txt", "b.txt", "c.txt"]);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p anodize-stage-archive test_archive_entries_sorted 2>&1 | tail -5`
Expected: compile error — `sort_entries` not defined

- [ ] **Step 3: Implement sort_entries**

Add near `deduplicate_entries`:

```rust
/// Sort archive entries by destination path for reproducible archives.
/// Matches GoReleaser's `slices.SortFunc` by destination in archivefiles.go.
fn sort_entries(mut entries: Vec<ArchiveEntry>) -> Vec<ArchiveEntry> {
    entries.sort_by(|a, b| a.archive_name.cmp(&b.archive_name));
    entries
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p anodize-stage-archive test_archive_entries_sorted 2>&1 | tail -5`
Expected: PASS

- [ ] **Step 5: Wire into archive stage**

In the `run()` method, after `deduplicate_entries`, add sorting:

```rust
let deduped = deduplicate_entries(combined);
let sorted = sort_entries(deduped);
let all_entries: Vec<&ArchiveEntry> = sorted.iter().collect();

let all_src_paths: Vec<PathBuf> = sorted.iter().map(|e| e.src.clone()).collect();
```

- [ ] **Step 6: Run all archive tests**

Run: `cargo test -p anodize-stage-archive 2>&1 | tail -20`
Expected: all tests pass

- [ ] **Step 7: Commit**

```bash
git add crates/stage-archive/src/lib.rs
git commit -m "feat(archive): sort entries by destination for reproducibility

Sort all archive entries by their destination path before writing,
matching GoReleaser's sort behavior in archivefiles.go."
```

---

### Task 4: Archive — template rendering for FileInfo fields

**Files:**
- Modify: `crates/stage-archive/src/lib.rs:938-946` (binary_info construction) and `977-1010` (extra_entries construction)

GoReleaser templates `owner`, `group`, `mtime` through the template engine via `tmplInfo()` (archivefiles.go:73-89). Anodize uses raw config values without rendering.

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn test_file_info_template_rendering() {
    use anodize_core::config::ArchiveFileInfo;
    use anodize_core::context::{Context, ContextOptions};
    use anodize_core::config::Config;

    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.2.3");
    ctx.template_vars_mut().set("ProjectName", "myapp");

    let info = ArchiveFileInfo {
        owner: Some("{{ .ProjectName }}".to_string()),
        group: Some("staff".to_string()),
        mode: Some("0755".to_string()),
        mtime: Some("{{ .Version }}".to_string()),
    };

    let rendered = render_file_info(&info, &ctx).unwrap();
    assert_eq!(rendered.owner.as_deref(), Some("myapp"));
    assert_eq!(rendered.group.as_deref(), Some("staff"));
    assert_eq!(rendered.mode.as_deref(), Some("0755"));
    assert_eq!(rendered.mtime.as_deref(), Some("1.2.3"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p anodize-stage-archive test_file_info_template 2>&1 | tail -5`
Expected: compile error — `render_file_info` not defined

- [ ] **Step 3: Implement render_file_info**

Add near `apply_file_info_to_header`:

```rust
/// Template-render the string fields of an ArchiveFileInfo (owner, group, mtime).
/// Mode is left as-is since it's an octal literal, not a template.
/// Matches GoReleaser's `tmplInfo()` in archivefiles.go.
fn render_file_info(
    info: &anodize_core::config::ArchiveFileInfo,
    ctx: &Context,
) -> Result<anodize_core::config::ArchiveFileInfo> {
    Ok(anodize_core::config::ArchiveFileInfo {
        owner: info
            .owner
            .as_deref()
            .map(|s| ctx.render_template(s))
            .transpose()?,
        group: info
            .group
            .as_deref()
            .map(|s| ctx.render_template(s))
            .transpose()?,
        mode: info.mode.clone(),
        mtime: info
            .mtime
            .as_deref()
            .map(|s| ctx.render_template(s))
            .transpose()?,
    })
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p anodize-stage-archive test_file_info_template 2>&1 | tail -5`
Expected: PASS

- [ ] **Step 5: Wire render_file_info into archive stage**

In the `run()` method, render `binary_info` after constructing it (around line 941):

```rust
let binary_info = archive_cfg.builds_info.clone().unwrap_or_else(|| {
    anodize_core::config::ArchiveFileInfo {
        mode: Some("0755".to_string()),
        ..Default::default()
    }
});
let binary_info = render_file_info(&binary_info, ctx)?;
```

And in the extra_entries construction (around line 1004-1007), render per-file info:

```rust
info: ef.info.as_ref().map(|i| render_file_info(i, ctx)).transpose()?,
```

Note: this changes the closure from `map` (infallible) to needing error handling. Convert the `.map(|ef| { ... })` closure to a fallible version using `.map(|ef| -> Result<ArchiveEntry> { ... }).collect::<Result<Vec<_>>>()`.

- [ ] **Step 6: Run all archive tests**

Run: `cargo test -p anodize-stage-archive 2>&1 | tail -20`
Expected: all tests pass

- [ ] **Step 7: Commit**

```bash
git add crates/stage-archive/src/lib.rs
git commit -m "feat(archive): template-render FileInfo owner/group/mtime fields

Render owner, group, and mtime through the template engine before
applying to archive entries, matching GoReleaser's tmplInfo()."
```

---

### Task 5: Archive — verify `binaries` filter is wired (test-only)

**Files:**
- Modify: `crates/stage-archive/src/lib.rs` (tests section only)

The `binaries` field IS already wired at lines 837-846. This task confirms it with a dedicated test.

- [ ] **Step 1: Write test confirming binaries filter works**

```rust
#[test]
fn test_archive_stage_binaries_filter() {
    use anodize_core::config::{ArchiveConfig, ArchivesConfig, Config, CrateConfig};
    use anodize_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    let bin_a = tmp.path().join("app-a");
    let bin_b = tmp.path().join("app-b");
    fs::write(&bin_a, b"binary-a").unwrap();
    fs::write(&bin_b, b"binary-b").unwrap();

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        archives: ArchivesConfig::Configs(vec![ArchiveConfig {
            name_template: Some("filtered-archive".to_string()),
            format: Some("tar.gz".to_string()),
            binaries: Some(vec!["app-a".to_string()]), // Only include app-a
            ..Default::default()
        }]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = dist.clone();
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");

    // Add two binaries with different names
    for (name, path) in [("app-a", &bin_a), ("app-b", &bin_b)] {
        let mut metadata = HashMap::new();
        metadata.insert("binary".to_string(), name.to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: path.clone(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata,
            size: None,
        });
    }

    let stage = ArchiveStage;
    stage.run(&mut ctx).unwrap();

    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(archives.len(), 1);

    // Verify only app-a is in the archive
    let archive_path = &archives[0].path;
    let file = File::open(archive_path).unwrap();
    let dec = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(dec);
    let names: Vec<String> = tar
        .entries()
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path().unwrap().to_string_lossy().to_string())
        .collect();

    assert!(names.iter().any(|n| n.contains("app-a")), "should contain app-a: {:?}", names);
    assert!(!names.iter().any(|n| n.contains("app-b")), "should NOT contain app-b: {:?}", names);
}
```

- [ ] **Step 2: Run test to verify it passes**

Run: `cargo test -p anodize-stage-archive test_archive_stage_binaries_filter 2>&1 | tail -10`
Expected: PASS (confirming the filter is already wired)

- [ ] **Step 3: Commit**

```bash
git add crates/stage-archive/src/lib.rs
git commit -m "test(archive): add test confirming binaries filter is wired

The binaries field was already correctly filtering binary artifacts
by name. This test documents that behavior."
```

---

### Task 6: Archive — Amd64 suffix in default name template

**Files:**
- Modify: `crates/stage-archive/src/lib.rs:614-620` (default_name_template and default_binary_name_template)

GoReleaser appends `{{ if not (eq .Amd64 "v1") }}{{ .Amd64 }}{{ end }}` to the default name template (archive.go:30). Anodize omits this.

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn test_default_name_template_includes_amd64_suffix() {
    let tmpl = default_name_template();
    assert!(
        tmpl.contains("Amd64"),
        "default template should contain Amd64 suffix, got: {}",
        tmpl
    );

    let tmpl_bin = default_binary_name_template();
    assert!(
        tmpl_bin.contains("Amd64"),
        "default binary template should contain Amd64 suffix, got: {}",
        tmpl_bin
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p anodize-stage-archive test_default_name_template_includes_amd64 2>&1 | tail -5`
Expected: FAIL — templates don't contain "Amd64"

- [ ] **Step 3: Update default templates**

GoReleaser Go template: `{{ if not (eq .Amd64 "v1") }}{{ .Amd64 }}{{ end }}`
Anodize uses Tera. The equivalent Tera syntax:

```rust
fn default_name_template() -> &'static str {
    "{{ .ProjectName }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}{% if Arm %}v{{ Arm }}{% endif %}{% if Mips %}_{{ Mips }}{% endif %}{% if Amd64 and Amd64 != \"v1\" %}{{ Amd64 }}{% endif %}"
}

fn default_binary_name_template() -> &'static str {
    "{{ .Binary }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}{% if Arm %}v{{ Arm }}{% endif %}{% if Mips %}_{{ Mips }}{% endif %}{% if Amd64 and Amd64 != \"v1\" %}{{ Amd64 }}{% endif %}"
}
```

Note: Check what Tera syntax the project uses for conditionals. The existing templates use `{% if Arm %}` which tests truthiness. The `and` operator and `!=` comparisons should be valid in Tera. If the template engine doesn't support `!=` directly, use `{% if Amd64 %}{% if Amd64 != "v1" %}{{ Amd64 }}{% endif %}{% endif %}` as a fallback.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p anodize-stage-archive test_default_name_template_includes_amd64 2>&1 | tail -5`
Expected: PASS

- [ ] **Step 5: Run a rendering test to verify Amd64 suffix actually renders**

Add a test that renders the template with Amd64 set to "v2":

```rust
#[test]
fn test_default_template_renders_amd64_v2_suffix() {
    use anodize_core::config::Config;
    use anodize_core::context::{Context, ContextOptions};

    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("ProjectName", "myapp");
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Os", "linux");
    ctx.template_vars_mut().set("Arch", "amd64");
    ctx.template_vars_mut().set("Amd64", "v2");

    let result = ctx.render_template(default_name_template()).unwrap();
    assert_eq!(result, "myapp_1.0.0_linux_amd64v2");
}

#[test]
fn test_default_template_omits_amd64_v1_suffix() {
    use anodize_core::config::Config;
    use anodize_core::context::{Context, ContextOptions};

    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("ProjectName", "myapp");
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Os", "linux");
    ctx.template_vars_mut().set("Arch", "amd64");
    ctx.template_vars_mut().set("Amd64", "v1");

    let result = ctx.render_template(default_name_template()).unwrap();
    assert_eq!(result, "myapp_1.0.0_linux_amd64");
}
```

- [ ] **Step 6: Run both tests**

Run: `cargo test -p anodize-stage-archive test_default_template_renders_amd64 test_default_template_omits_amd64 2>&1 | tail -10`
Expected: PASS

- [ ] **Step 7: Run all archive tests for regressions**

Run: `cargo test -p anodize-stage-archive 2>&1 | tail -20`
Expected: all pass. If any existing tests break because they assert exact archive names without Amd64, update them to either set `Amd64` to "v1" or adjust expected names.

- [ ] **Step 8: Commit**

```bash
git add crates/stage-archive/src/lib.rs
git commit -m "feat(archive): add Amd64 version suffix to default name template

Include conditional Amd64 suffix (omitted for v1) in both archive
and binary default name templates, matching GoReleaser's behavior."
```

---

### Task 7: Archive — support UniversalBinary/Header/CArchive/CShared artifact types

**Files:**
- Modify: `crates/core/src/artifact.rs` (add `by_kinds_and_crate` method)
- Modify: `crates/stage-archive/src/lib.rs:739-744` (artifact query)

GoReleaser archives `Binary`, `UniversalBinary`, `Header`, `CArchive`, and `CShared` (archive.go:120-124). Anodize only queries `Binary`.

- [ ] **Step 1: Write failing test for by_kinds_and_crate**

In `crates/core/src/artifact.rs` tests:

```rust
#[test]
fn test_by_kinds_and_crate() {
    let mut registry = ArtifactStore::new();
    registry.add(Artifact {
        kind: ArtifactKind::Binary,
        name: "bin".to_string(),
        path: PathBuf::from("bin"),
        target: None,
        crate_name: "app".to_string(),
        metadata: HashMap::new(),
        size: None,
    });
    registry.add(Artifact {
        kind: ArtifactKind::UniversalBinary,
        name: "ubin".to_string(),
        path: PathBuf::from("ubin"),
        target: None,
        crate_name: "app".to_string(),
        metadata: HashMap::new(),
        size: None,
    });
    registry.add(Artifact {
        kind: ArtifactKind::Header,
        name: "hdr".to_string(),
        path: PathBuf::from("hdr"),
        target: None,
        crate_name: "other".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    let results = registry.by_kinds_and_crate(
        &[ArtifactKind::Binary, ArtifactKind::UniversalBinary],
        "app",
    );
    assert_eq!(results.len(), 2);

    let results = registry.by_kinds_and_crate(
        &[ArtifactKind::Header],
        "app",
    );
    assert_eq!(results.len(), 0);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p anodize-core test_by_kinds_and_crate 2>&1 | tail -5`
Expected: compile error — `by_kinds_and_crate` not defined

- [ ] **Step 3: Implement by_kinds_and_crate**

Add to `impl ArtifactStore` in `crates/core/src/artifact.rs`, next to `by_kind_and_crate`:

```rust
pub fn by_kinds_and_crate(&self, kinds: &[ArtifactKind], crate_name: &str) -> Vec<&Artifact> {
    self.artifacts
        .iter()
        .filter(|a| kinds.contains(&a.kind) && a.crate_name == crate_name)
        .collect()
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p anodize-core test_by_kinds_and_crate 2>&1 | tail -5`
Expected: PASS

- [ ] **Step 5: Update archive stage to query all 5 artifact types**

In `crates/stage-archive/src/lib.rs`, replace the single-kind query (around line 739-744):

Replace:
```rust
let all_binaries: Vec<Artifact> = ctx
    .artifacts
    .by_kind_and_crate(ArtifactKind::Binary, crate_name)
    .into_iter()
    .cloned()
    .collect();
```

With:
```rust
// Archive all build artifact types, matching GoReleaser
// (Binary, UniversalBinary, Header, CArchive, CShared).
let archivable_kinds = [
    ArtifactKind::Binary,
    ArtifactKind::UniversalBinary,
    ArtifactKind::Header,
    ArtifactKind::CArchive,
    ArtifactKind::CShared,
];
let all_binaries: Vec<Artifact> = ctx
    .artifacts
    .by_kinds_and_crate(&archivable_kinds, crate_name)
    .into_iter()
    .cloned()
    .collect();
```

- [ ] **Step 6: Run all archive tests**

Run: `cargo test -p anodize-stage-archive 2>&1 | tail -20`
Expected: all pass

- [ ] **Step 7: Commit**

```bash
git add crates/core/src/artifact.rs crates/stage-archive/src/lib.rs
git commit -m "feat(archive): archive UniversalBinary/Header/CArchive/CShared types

Add by_kinds_and_crate() to ArtifactStore and expand archive stage
to include all 5 archivable artifact types matching GoReleaser."
```

---

### Task 8: Source — implement strip_parent

**Files:**
- Modify: `crates/stage-source/src/lib.rs:92-110` (extra files loop)

The `strip_parent` field is parsed but logs "not yet supported". When `strip_parent` is true, the file should be placed at the archive root (filename only, no parent dirs). The current fallback to `file_name()` already does this by default for files without `dst`, but when `dst` IS set alongside `strip_parent`, GoReleaser uses `dst/filename` (archivefiles.go:113-114).

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn test_source_extra_files_strip_parent() {
    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    // Create a nested file
    let nested_dir = tmp.path().join("deep").join("nested");
    fs::create_dir_all(&nested_dir).unwrap();
    let nested_file = nested_dir.join("config.toml");
    fs::write(&nested_file, b"[settings]\nfoo = true").unwrap();

    // Initialize a git repo so git archive works
    let repo_dir = tmp.path();
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(repo_dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(repo_dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "init", "--allow-empty"])
        .current_dir(repo_dir)
        .output()
        .unwrap();

    let extra_files = vec![anodize_core::config::SourceFileEntry {
        src: nested_file.to_string_lossy().to_string(),
        dst: None,
        strip_parent: Some(true),
        info: None,
    }];

    let log = anodize_core::log::StageLogger::new("source", false);
    let result = create_source_archive(
        &dist, "tar.gz", "test-src", "test-src",
        &extra_files, repo_dir, "HEAD", &log,
    );
    assert!(result.is_ok(), "create_source_archive failed: {:?}", result.err());

    // Verify: the file should appear as test-src/config.toml (stripped parent)
    // not test-src/deep/nested/config.toml
    let archive_path = result.unwrap();
    let file = File::open(&archive_path).unwrap();
    let dec = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(dec);
    let names: Vec<String> = tar
        .entries()
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path().unwrap().to_string_lossy().to_string())
        .collect();

    assert!(
        names.iter().any(|n| n == "test-src/config.toml"),
        "should contain test-src/config.toml (stripped), got: {:?}",
        names
    );
    assert!(
        !names.iter().any(|n| n.contains("deep/nested")),
        "should NOT contain deep/nested path, got: {:?}",
        names
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p anodize-stage-source test_source_extra_files_strip_parent 2>&1 | tail -10`
Expected: FAIL or the test might already pass since fallback uses `file_name()`. If it passes, the real gap is when `dst` is also set — adjust test to use `dst: Some("configs".to_string())` and assert the file appears as `test-src/configs/config.toml`.

- [ ] **Step 3: Implement strip_parent properly and remove warning**

In `crates/stage-source/src/lib.rs`, replace lines 92-110:

```rust
for entry in extra_files {
    let src = Path::new(&entry.src);
    let do_strip = entry.strip_parent.unwrap_or(false);

    let dest_name: std::ffi::OsString = if let Some(ref dst) = entry.dst {
        if do_strip {
            // strip_parent + dst: place filename directly under dst
            let fname = src
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("source: extra file has no filename: {}", entry.src))?;
            PathBuf::from(dst).join(fname).into_os_string()
        } else {
            std::ffi::OsString::from(dst)
        }
    } else if do_strip {
        // strip_parent without dst: use filename only
        src.file_name()
            .ok_or_else(|| anyhow::anyhow!("source: extra file has no filename: {}", entry.src))?
            .to_os_string()
    } else {
        src.file_name()
            .ok_or_else(|| anyhow::anyhow!("source: extra file has no filename: {}", entry.src))?
            .to_os_string()
    };

    // Ensure parent directories exist for nested destinations
    let full_dest = prefixed_dir.join(&dest_name);
    if let Some(parent) = full_dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("source: create parent dirs for '{}'", full_dest.display()))?;
    }

    std::fs::copy(src, &full_dest)
        .with_context(|| format!("source: copy extra file '{}' to staging", entry.src))?;
}
```

Note: The old warning `log.warn("strip_parent is not yet supported...")` is removed entirely.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p anodize-stage-source test_source_extra_files_strip_parent 2>&1 | tail -10`
Expected: PASS

- [ ] **Step 5: Run all source tests**

Run: `cargo test -p anodize-stage-source 2>&1 | tail -20`
Expected: all pass

- [ ] **Step 6: Commit**

```bash
git add crates/stage-source/src/lib.rs
git commit -m "feat(source): implement strip_parent for extra files

Replace 'not yet supported' warning with actual implementation.
When strip_parent is true, files are placed at the archive root
(or under dst) with parent directories stripped."
```

---

### Task 9: Source — implement file metadata (info) via tar crate

**Files:**
- Modify: `crates/stage-source/Cargo.toml` (add tar, flate2 dependencies)
- Modify: `crates/stage-source/src/lib.rs:85-163` (replace shell tar/gzip with Rust tar crate for extra files)

The `info` field (owner, group, mode, mtime) is parsed but logs "not yet supported". The current implementation uses shell `tar --append` + `gzip` commands. To apply per-file metadata, we need programmatic tar header control. Replace the shell commands with Rust `tar` crate operations for the extra-files append step.

- [ ] **Step 1: Add tar and flate2 dependencies**

In `crates/stage-source/Cargo.toml`, add to `[dependencies]`:

```toml
tar.workspace = true
flate2.workspace = true
```

- [ ] **Step 2: Write failing test**

```rust
#[test]
fn test_source_extra_files_with_info() {
    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let extra_file = tmp.path().join("config.toml");
    fs::write(&extra_file, b"[settings]\nfoo = true").unwrap();

    // Init git repo
    let repo_dir = tmp.path();
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(repo_dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(repo_dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["-c", "user.email=test@test.com", "-c", "user.name=Test",
               "commit", "-m", "init"])
        .current_dir(repo_dir)
        .output()
        .unwrap();

    let extra_files = vec![anodize_core::config::SourceFileEntry {
        src: extra_file.to_string_lossy().to_string(),
        dst: None,
        strip_parent: None,
        info: Some(anodize_core::config::SourceFileInfo {
            owner: Some("deploy".to_string()),
            group: Some("staff".to_string()),
            mode: Some(0o644),
            mtime: Some("2024-01-01T00:00:00Z".to_string()),
        }),
    }];

    let log = anodize_core::log::StageLogger::new("source", false);
    let result = create_source_archive(
        &dist, "tar.gz", "test-src", "test-src",
        &extra_files, repo_dir, "HEAD", &log,
    );
    assert!(result.is_ok(), "failed: {:?}", result.err());

    // Read back and verify metadata
    let archive_path = result.unwrap();
    let file = std::fs::File::open(&archive_path).unwrap();
    let dec = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(dec);

    for entry in tar.entries().unwrap() {
        let entry = entry.unwrap();
        let path = entry.path().unwrap().to_string_lossy().to_string();
        if path.ends_with("config.toml") {
            let header = entry.header();
            assert_eq!(header.mode().unwrap(), 0o644);
            assert_eq!(header.username().unwrap().unwrap(), "deploy");
            assert_eq!(header.groupname().unwrap().unwrap(), "staff");
            // 2024-01-01T00:00:00Z = 1704067200 unix timestamp
            assert_eq!(header.mtime().unwrap(), 1704067200);
            return;
        }
    }
    panic!("config.toml not found in source archive");
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p anodize-stage-source test_source_extra_files_with_info 2>&1 | tail -10`
Expected: FAIL — metadata not applied (owner/mode will be from filesystem defaults)

- [ ] **Step 4: Rewrite extra files append to use Rust tar crate**

Replace the shell-based `tar --append` + `gzip` block (lines ~85-163) with programmatic tar manipulation. The new approach:

1. After `git archive` creates the initial tar (always uncompressed for post-processing)
2. Open the tar with Rust `tar` crate in append mode
3. For each extra file, build a header with metadata from `entry.info`
4. Append to tar
5. Compress if needed

```rust
// Append extra files using the tar crate for metadata control
if needs_post_append {
    use std::io::Read as _;

    // Open the initial tar for appending
    let tar_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&initial_path)
        .context("source: open tar for appending extra files")?;
    let mut tar_builder = tar::Builder::new(tar_file);

    for entry in extra_files {
        let src = Path::new(&entry.src);
        let do_strip = entry.strip_parent.unwrap_or(false);

        // Compute destination name inside the prefix
        let dest_rel: PathBuf = if let Some(ref dst) = entry.dst {
            if do_strip {
                let fname = src.file_name().ok_or_else(|| {
                    anyhow::anyhow!("source: extra file has no filename: {}", entry.src)
                })?;
                PathBuf::from(dst).join(fname)
            } else {
                PathBuf::from(dst)
            }
        } else if do_strip {
            PathBuf::from(
                src.file_name().ok_or_else(|| {
                    anyhow::anyhow!("source: extra file has no filename: {}", entry.src)
                })?,
            )
        } else {
            PathBuf::from(
                src.file_name().ok_or_else(|| {
                    anyhow::anyhow!("source: extra file has no filename: {}", entry.src)
                })?,
            )
        };

        let archive_path = Path::new(prefix).join(&dest_rel);

        // Read file content
        let mut file_data = Vec::new();
        std::fs::File::open(src)
            .with_context(|| format!("source: open extra file '{}'", entry.src))?
            .read_to_end(&mut file_data)
            .with_context(|| format!("source: read extra file '{}'", entry.src))?;

        // Build tar header
        let metadata = std::fs::metadata(src)
            .with_context(|| format!("source: metadata for '{}'", entry.src))?;
        let mut header = tar::Header::new_gnu();
        header.set_size(file_data.len() as u64);
        header.set_mode(metadata.permissions().mode());
        header.set_mtime(
            metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0),
        );

        // Apply info overrides if present
        if let Some(ref info) = entry.info {
            if let Some(ref owner) = info.owner {
                header.set_username(owner).ok();
            }
            if let Some(ref group) = info.group {
                header.set_groupname(group).ok();
            }
            if let Some(mode) = info.mode {
                header.set_mode(mode);
            }
            if let Some(ref mtime_str) = info.mtime {
                if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(mtime_str) {
                    header.set_mtime(dt.timestamp() as u64);
                } else if let Ok(ts) = mtime_str.parse::<u64>() {
                    header.set_mtime(ts);
                } else {
                    log.warn(&format!(
                        "could not parse mtime '{}' as RFC3339 or unix timestamp",
                        mtime_str
                    ));
                }
            }
        }

        header.set_path(&archive_path).with_context(|| {
            format!("source: set tar path for '{}'", archive_path.display())
        })?;
        header.set_cksum();

        tar_builder
            .append(&header, &file_data[..])
            .with_context(|| format!("source: append '{}' to tar", entry.src))?;
    }

    tar_builder.finish().context("source: finish tar")?;
    drop(tar_builder);

    // Compress if needed
    if git_format == "tar.gz" {
        let tar_data = std::fs::read(&initial_path)
            .context("source: read tar for gzip compression")?;
        let gz_file = std::fs::File::create(&output_path)
            .context("source: create gzip output file")?;
        let mut encoder = flate2::write::GzEncoder::new(gz_file, flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, &tar_data)
            .context("source: write gzip data")?;
        encoder.finish().context("source: finish gzip")?;
        let _ = std::fs::remove_file(&initial_path);
    } else {
        std::fs::rename(&initial_path, &output_path).with_context(|| {
            format!(
                "source: rename {} -> {}",
                initial_path.display(),
                output_path.display()
            )
        })?;
    }
}
```

Also remove the old `log.warn("file info ... not yet supported")` line.

Also add `use std::os::unix::fs::PermissionsExt;` at the top of the file (needed for `.mode()` on Unix).

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p anodize-stage-source test_source_extra_files_with_info 2>&1 | tail -10`
Expected: PASS

- [ ] **Step 6: Run all source tests**

Run: `cargo test -p anodize-stage-source 2>&1 | tail -20`
Expected: all pass

- [ ] **Step 7: Run full workspace build**

Run: `cargo build 2>&1 | tail -10`
Expected: clean build

- [ ] **Step 8: Commit**

```bash
git add crates/stage-source/Cargo.toml crates/stage-source/src/lib.rs
git commit -m "feat(source): implement file metadata (info) for extra files

Replace shell tar/gzip commands with Rust tar crate for extra file
appending, enabling per-file metadata control (owner, group, mode,
mtime). Remove 'not yet supported' warnings for both strip_parent
and file info."
```

---

### Task 10: Final integration verification

**Files:** None (test-only)

- [ ] **Step 1: Run full test suite for both crates**

Run: `cargo test -p anodize-stage-archive -p anodize-stage-source -p anodize-core 2>&1 | tail -30`
Expected: all tests pass

- [ ] **Step 2: Run cargo clippy**

Run: `cargo clippy -p anodize-stage-archive -p anodize-stage-source -p anodize-core 2>&1 | tail -20`
Expected: no warnings

- [ ] **Step 3: Update parity session index**

Mark all Session I items as checked in `/opt/repos/anodize/.claude/specs/parity-session-index.md`:
- Change all `- [ ]` under Session I to `- [x]`

- [ ] **Step 4: Commit**

```bash
git add .claude/specs/parity-session-index.md
git commit -m "feat(archive,source): Session I — archive & source behavioral gaps

Implements 9 behavioral parity fixes:
- Archive: LCP-based directory preservation for glob destinations
- Archive: duplicate destination path detection with warnings
- Archive: file sorting by destination for reproducibility
- Archive: template rendering for FileInfo owner/group/mtime
- Archive: verified binaries filter is already wired (added test)
- Archive: Amd64 version suffix in default name template
- Archive: support UniversalBinary/Header/CArchive/CShared types
- Source: implement strip_parent for extra files
- Source: implement file metadata via Rust tar crate"
```
