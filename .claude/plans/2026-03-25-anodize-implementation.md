# Anodize Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a Rust-native GoReleaser alternative that reads a declarative config and executes a full release pipeline (build, archive, checksum, changelog, GitHub release, Docker, Homebrew, Scoop, crates.io, nFPM, signing, announce).

**Architecture:** Cargo workspace with a `core` crate (Stage trait, Context, template engine, config schema, artifact registry) and per-stage crates (`stage-build`, `stage-archive`, etc.), all compiled into a single `anodize` binary via a `cli` crate. Stages are stateless; all mutable state lives in `Context`.

**Tech Stack:** Rust 2024 edition, clap (CLI), serde + serde_yaml + toml (config), octocrab (GitHub API), anyhow (errors), sha2 (checksums), flate2 + tar + zip (archives), regex (changelog/templates), reqwest (webhooks), colored (terminal output).

**Spec:** `/opt/repos/anodize/.claude/specs/2026-03-25-anodize-design.md`

---

## File Structure

```
/opt/repos/anodize/
├── Cargo.toml                          # Workspace root
├── crates/
│   ├── core/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs                  # Re-exports
│   │       ├── config.rs               # Config schema (serde structs)
│   │       ├── context.rs              # Context struct
│   │       ├── stage.rs                # Stage trait
│   │       ├── artifact.rs             # Artifact registry
│   │       ├── template.rs             # Go-style template engine
│   │       ├── git.rs                  # Git state detection
│   │       └── target.rs              # Target triple → Os/Arch mapping
│   ├── stage-build/
│   │   ├── Cargo.toml
│   │   └── src/lib.rs                  # Build stage (cargo/zigbuild/cross)
│   ├── stage-archive/
│   │   ├── Cargo.toml
│   │   └── src/lib.rs                  # Archive stage (tar.gz/zip)
│   ├── stage-nfpm/
│   │   ├── Cargo.toml
│   │   └── src/lib.rs                  # NFpm stage (.deb/.rpm/.apk)
│   ├── stage-checksum/
│   │   ├── Cargo.toml
│   │   └── src/lib.rs                  # Checksum stage (SHA256/512)
│   ├── stage-changelog/
│   │   ├── Cargo.toml
│   │   └── src/lib.rs                  # Changelog generation
│   ├── stage-release/
│   │   ├── Cargo.toml
│   │   └── src/lib.rs                  # GitHub Release creation
│   ├── stage-publish/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs                  # Re-exports
│   │       ├── crates_io.rs            # crates.io publishing
│   │       ├── homebrew.rs             # Homebrew tap
│   │       └── scoop.rs                # Scoop bucket
│   ├── stage-docker/
│   │   ├── Cargo.toml
│   │   └── src/lib.rs                  # Docker buildx
│   ├── stage-sign/
│   │   ├── Cargo.toml
│   │   └── src/lib.rs                  # GPG/cosign signing
│   ├── stage-announce/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs                  # Re-exports
│   │       ├── discord.rs              # Discord webhook
│   │       ├── slack.rs                # Slack webhook
│   │       └── webhook.rs              # Generic HTTP webhook
│   └── cli/
│       ├── Cargo.toml
│       └── src/
│           ├── main.rs                 # Entry point
│           ├── commands/
│           │   ├── mod.rs
│           │   ├── release.rs          # release command
│           │   ├── build.rs            # build-only command
│           │   ├── check.rs            # config validation
│           │   ├── init.rs             # config generation
│           │   └── changelog.rs        # changelog-only command
│           └── pipeline.rs             # Pipeline assembly and execution
```

---

## Phase 1: Foundation

### Task 1: Project Scaffolding

**Files:**
- Create: `Cargo.toml` (workspace root)
- Create: `crates/core/Cargo.toml`
- Create: `crates/core/src/lib.rs`
- Create: `crates/cli/Cargo.toml`
- Create: `crates/cli/src/main.rs`
- Create: All stage crate `Cargo.toml` and `src/lib.rs` stubs

- [ ] **Step 1: Create workspace root Cargo.toml**

```toml
[workspace]
resolver = "2"
members = [
    "crates/core",
    "crates/stage-build",
    "crates/stage-archive",
    "crates/stage-nfpm",
    "crates/stage-checksum",
    "crates/stage-changelog",
    "crates/stage-release",
    "crates/stage-publish",
    "crates/stage-docker",
    "crates/stage-sign",
    "crates/stage-announce",
    "crates/cli",
]

[workspace.package]
version = "0.1.0"
edition = "2024"
license = "MIT"
repository = "https://github.com/tj-smith47/anodize"
authors = ["TJ Smith"]

[workspace.dependencies]
anodize-core = { path = "crates/core" }
anyhow = "1"
serde = { version = "1", features = ["derive"] }
serde_yaml = "0.9"
toml = "0.8"
```

- [ ] **Step 2: Create core crate**

`crates/core/Cargo.toml`:
```toml
[package]
name = "anodize-core"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
anyhow.workspace = true
serde = { workspace = true, features = ["derive"] }
serde_yaml.workspace = true
toml.workspace = true
```

`crates/core/src/lib.rs`:
```rust
pub mod artifact;
pub mod config;
pub mod context;
pub mod git;
pub mod stage;
pub mod target;
pub mod template;
```

- [ ] **Step 3: Create all stage crate stubs**

Each stage crate gets a `Cargo.toml` depending on `anodize-core` and a `src/lib.rs` with a placeholder struct implementing `Stage`. Example for `stage-build`:

`crates/stage-build/Cargo.toml`:
```toml
[package]
name = "anodize-stage-build"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
anodize-core.workspace = true
anyhow.workspace = true
```

`crates/stage-build/src/lib.rs`:
```rust
use anodize_core::stage::Stage;
use anodize_core::context::Context;
use anyhow::Result;

pub struct BuildStage;

impl Stage for BuildStage {
    fn name(&self) -> &str { "build" }
    fn run(&self, _ctx: &mut Context) -> Result<()> { todo!() }
}
```

Repeat for: `stage-archive`, `stage-nfpm`, `stage-checksum`, `stage-changelog`, `stage-release`, `stage-publish`, `stage-docker`, `stage-sign`, `stage-announce`.

- [ ] **Step 4: Create cli crate**

`crates/cli/Cargo.toml`:
```toml
[package]
name = "anodize"
version.workspace = true
edition.workspace = true
license.workspace = true

[[bin]]
name = "anodize"
path = "src/main.rs"

[dependencies]
anodize-core.workspace = true
anodize-stage-build = { path = "../stage-build" }
anodize-stage-archive = { path = "../stage-archive" }
anodize-stage-nfpm = { path = "../stage-nfpm" }
anodize-stage-checksum = { path = "../stage-checksum" }
anodize-stage-changelog = { path = "../stage-changelog" }
anodize-stage-release = { path = "../stage-release" }
anodize-stage-publish = { path = "../stage-publish" }
anodize-stage-docker = { path = "../stage-docker" }
anodize-stage-sign = { path = "../stage-sign" }
anodize-stage-announce = { path = "../stage-announce" }
anyhow.workspace = true
clap = { version = "4", features = ["derive"] }
```

`crates/cli/src/main.rs`:
```rust
fn main() {
    println!("anodize v0.1.0");
}
```

- [ ] **Step 5: Verify workspace compiles**

Run: `cd /opt/repos/anodize && cargo build`
Expected: Builds successfully with no errors.

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "feat: scaffold workspace with core and stage crate stubs"
```

---

### Task 2: Core — Config Schema

**Files:**
- Create: `crates/core/src/config.rs`
- Test: `crates/core/src/config.rs` (inline tests)

- [ ] **Step 1: Write tests for config deserialization**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_minimal_yaml_config() {
        let yaml = r#"
project_name: myproject
crates:
  - name: myproject
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.project_name, "myproject");
        assert_eq!(config.crates.len(), 1);
        assert_eq!(config.dist, PathBuf::from("./dist"));
    }

    #[test]
    fn test_minimal_toml_config() {
        let toml_str = r#"
project_name = "myproject"

[[crates]]
name = "myproject"
path = "."
tag_template = "v{{ .Version }}"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.project_name, "myproject");
    }

    #[test]
    fn test_full_config_with_defaults() {
        let yaml = r#"
project_name: cfgd
dist: ./dist
defaults:
  targets:
    - x86_64-unknown-linux-gnu
    - aarch64-apple-darwin
  cross: auto
  flags: --release
  archives:
    format: tar.gz
    format_overrides:
      - os: windows
        format: zip
  checksum:
    algorithm: sha256
crates:
  - name: cfgd
    path: crates/cfgd
    tag_template: "v{{ .Version }}"
    builds:
      - binary: cfgd
        features: []
        no_default_features: false
    archives:
      - name_template: "{{ .ProjectName }}-{{ .Version }}-{{ .Os }}-{{ .Arch }}"
        files:
          - LICENSE
    release:
      github:
        owner: tj-smith47
        name: cfgd
      draft: false
      prerelease: auto
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let defaults = config.defaults.unwrap();
        assert_eq!(defaults.targets.unwrap().len(), 2);
        assert_eq!(defaults.cross, Some(CrossStrategy::Auto));
        let release = config.crates[0].release.as_ref().unwrap();
        assert_eq!(release.name_template, Some("{{ .Tag }}".to_string()));
    }

    #[test]
    fn test_snapshot_config() {
        let yaml = r#"
project_name: test
snapshot:
  name_template: "{{ .Version }}-SNAPSHOT-{{ .ShortCommit }}"
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            config.snapshot.unwrap().name_template,
            "{{ .Version }}-SNAPSHOT-{{ .ShortCommit }}"
        );
    }

    #[test]
    fn test_archives_false() {
        let yaml = r#"
project_name: test
crates:
  - name: operator
    path: crates/operator
    tag_template: "v{{ .Version }}"
    archives: false
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(config.crates[0].archives, ArchivesConfig::Disabled));
    }

    #[test]
    fn test_publish_crates_bool_and_object() {
        let yaml_bool = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      crates: true
"#;
        let config: Config = serde_yaml::from_str(yaml_bool).unwrap();
        assert!(config.crates[0].publish.as_ref().unwrap().crates_config().enabled);

        let yaml_obj = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      crates:
        enabled: true
        index_timeout: 120
"#;
        let config: Config = serde_yaml::from_str(yaml_obj).unwrap();
        let crates_cfg = config.crates[0].publish.as_ref().unwrap().crates_config();
        assert!(crates_cfg.enabled);
        assert_eq!(crates_cfg.index_timeout, 120);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /opt/repos/anodize && cargo test -p anodize-core`
Expected: FAIL — structs not defined.

- [ ] **Step 3: Implement Config structs**

Implement all serde-deserializable config structs in `crates/core/src/config.rs`:
- `Config` (top-level: project_name, dist, defaults, before, after, crates, changelog, sign, docker_signs, snapshot, announce)
- `Defaults` (targets, cross, flags, archives, checksum)
- `CrateConfig` (name, path, tag_template, depends_on, builds, archives, checksum, release, publish, docker, nfpm)
- `BuildConfig` (binary, targets, features, no_default_features, env, copy_from, flags)
- `ArchivesConfig` (enum: Disabled | Configs(Vec<ArchiveConfig>))
- `ArchiveConfig` (name_template, format, format_overrides, files, binaries)
- `FormatOverride` (os, format)
- `ChecksumConfig` (name_template, algorithm)
- `ReleaseConfig` (github, draft, prerelease, name_template)
- `GitHubConfig` (owner, name)
- `PublishConfig` (crates, homebrew, scoop)
- `CratesPublishConfig` (enum: bool or object with enabled + index_timeout)
- `HomebrewConfig` (tap, folder, description, license, install, test)
- `ScoopConfig` (bucket, description)
- `TapConfig` / `BucketConfig` (owner, name)
- `DockerConfig` (image_templates, dockerfile, platforms, binaries, build_flag_templates)
- `NfpmConfig` (package_name, formats, vendor, homepage, maintainer, description, license, bindir, contents, dependencies, overrides)
- `ChangelogConfig` (sort, filters, groups)
- `SignConfig` (artifacts, cmd, args)
- `DockerSignConfig` (artifacts, cmd, args)
- `SnapshotConfig` (name_template)
- `AnnounceConfig` (discord, slack, webhook)
- `HooksConfig` (hooks: Vec<String>)
- `CrossStrategy` enum (Auto, Zigbuild, Cross, Cargo)
- `PrereleaseConfig` enum (Auto, Bool)

Use `#[serde(default)]` for optional fields with sensible defaults. Use `#[serde(untagged)]` for union types (ArchivesConfig, CratesPublishConfig).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd /opt/repos/anodize && cargo test -p anodize-core`
Expected: All pass.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(core): config schema with serde deserialization"
```

---

### Task 3: Core — Target Triple Mapping

**Files:**
- Create: `crates/core/src/target.rs`
- Test: inline tests

- [ ] **Step 1: Write tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_target_to_os_arch() {
        let (os, arch) = map_target("x86_64-unknown-linux-gnu");
        assert_eq!(os, "linux");
        assert_eq!(arch, "amd64");
    }

    #[test]
    fn test_darwin_arm64() {
        let (os, arch) = map_target("aarch64-apple-darwin");
        assert_eq!(os, "darwin");
        assert_eq!(arch, "arm64");
    }

    #[test]
    fn test_windows() {
        let (os, arch) = map_target("x86_64-pc-windows-msvc");
        assert_eq!(os, "windows");
        assert_eq!(arch, "amd64");
    }

    #[test]
    fn test_unknown_target() {
        let (os, arch) = map_target("riscv64gc-unknown-linux-gnu");
        assert_eq!(os, "linux");
        assert_eq!(arch, "riscv64gc");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /opt/repos/anodize && cargo test -p anodize-core target`
Expected: FAIL

- [ ] **Step 3: Implement target mapping**

```rust
pub fn map_target(triple: &str) -> (String, String) {
    let parts: Vec<&str> = triple.split('-').collect();
    let arch = match parts.first().copied().unwrap_or("unknown") {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "i686" => "386",
        "armv7" => "armv7",
        other => other,
    };
    let os = if triple.contains("linux") {
        "linux"
    } else if triple.contains("darwin") || triple.contains("apple") {
        "darwin"
    } else if triple.contains("windows") {
        "windows"
    } else if triple.contains("freebsd") {
        "freebsd"
    } else {
        "unknown"
    };
    (os.to_string(), arch.to_string())
}
```

- [ ] **Step 4: Run tests, verify pass**

Run: `cd /opt/repos/anodize && cargo test -p anodize-core target`
Expected: All pass.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(core): target triple to OS/arch mapping"
```

---

### Task 4: Core — Template Engine

**Files:**
- Create: `crates/core/src/template.rs`
- Test: inline tests

- [ ] **Step 1: Write tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn test_vars() -> TemplateVars {
        let mut vars = TemplateVars::new();
        vars.set("ProjectName", "cfgd");
        vars.set("Version", "1.2.3");
        vars.set("Tag", "v1.2.3");
        vars.set("Os", "linux");
        vars.set("Arch", "amd64");
        vars.set("ShortCommit", "abc1234");
        vars.set("Major", "1");
        vars.set("Minor", "2");
        vars.set("Patch", "3");
        vars.set_env("GITHUB_TOKEN", "tok123");
        vars
    }

    #[test]
    fn test_simple_substitution() {
        let vars = test_vars();
        let result = render("{{ .ProjectName }}-{{ .Version }}", &vars).unwrap();
        assert_eq!(result, "cfgd-1.2.3");
    }

    #[test]
    fn test_env_access() {
        let vars = test_vars();
        let result = render("{{ .Env.GITHUB_TOKEN }}", &vars).unwrap();
        assert_eq!(result, "tok123");
    }

    #[test]
    fn test_no_spaces() {
        let vars = test_vars();
        let result = render("{{.ProjectName}}-{{.Version}}", &vars).unwrap();
        assert_eq!(result, "cfgd-1.2.3");
    }

    #[test]
    fn test_missing_var() {
        let vars = test_vars();
        let result = render("{{ .Missing }}", &vars);
        assert!(result.is_err());
    }

    #[test]
    fn test_archive_name_template() {
        let vars = test_vars();
        let result = render("{{ .ProjectName }}-{{ .Version }}-{{ .Os }}-{{ .Arch }}", &vars).unwrap();
        assert_eq!(result, "cfgd-1.2.3-linux-amd64");
    }

    #[test]
    fn test_literal_text_preserved() {
        let vars = test_vars();
        let result = render("prefix-{{ .Tag }}-suffix.tar.gz", &vars).unwrap();
        assert_eq!(result, "prefix-v1.2.3-suffix.tar.gz");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /opt/repos/anodize && cargo test -p anodize-core template`
Expected: FAIL

- [ ] **Step 3: Implement template engine**

Implement `TemplateVars` struct with a `HashMap<String, String>` for dot-access variables and a nested `HashMap<String, String>` for `Env`. Public API:
- `pub fn new() -> Self`
- `pub fn set(&mut self, key: &str, value: &str)`
- `pub fn get(&self, key: &str) -> Option<&String>`
- `pub fn set_env(&mut self, key: &str, value: &str)`

Implement `render(template: &str, vars: &TemplateVars) -> Result<String>` that uses regex to find `\{\{\s*\.(\w+(?:\.\w+)*)\s*\}\}` patterns and replaces them with values from the vars.

- [ ] **Step 4: Run tests, verify pass**

Run: `cd /opt/repos/anodize && cargo test -p anodize-core template`
Expected: All pass.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(core): Go-style template engine"
```

---

### Task 5: Core — Artifact Registry

**Files:**
- Create: `crates/core/src/artifact.rs`
- Test: inline tests

- [ ] **Step 1: Write tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_add_and_query_artifacts() {
        let mut registry = ArtifactRegistry::new();
        registry.add(Artifact {
            kind: ArtifactKind::Binary,
            path: PathBuf::from("dist/cfgd"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "cfgd".to_string(),
            metadata: Default::default(),
        });
        registry.add(Artifact {
            kind: ArtifactKind::Archive,
            path: PathBuf::from("dist/cfgd.tar.gz"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "cfgd".to_string(),
            metadata: Default::default(),
        });

        let binaries = registry.by_kind(ArtifactKind::Binary);
        assert_eq!(binaries.len(), 1);

        let archives = registry.by_kind_and_crate(ArtifactKind::Archive, "cfgd");
        assert_eq!(archives.len(), 1);
    }

    #[test]
    fn test_empty_query() {
        let registry = ArtifactRegistry::new();
        assert!(registry.by_kind(ArtifactKind::Binary).is_empty());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /opt/repos/anodize && cargo test -p anodize-core artifact`
Expected: FAIL

- [ ] **Step 3: Implement**

```rust
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArtifactKind {
    Binary,
    Archive,
    Checksum,
    DockerImage,
    LinuxPackage,
    Metadata,
}

#[derive(Debug, Clone)]
pub struct Artifact {
    pub kind: ArtifactKind,
    pub path: PathBuf,
    pub target: Option<String>,
    pub crate_name: String,
    pub metadata: HashMap<String, String>,
}

#[derive(Debug, Default)]
pub struct ArtifactRegistry {
    artifacts: Vec<Artifact>,
}

impl ArtifactRegistry {
    pub fn new() -> Self { Self::default() }

    pub fn add(&mut self, artifact: Artifact) {
        self.artifacts.push(artifact);
    }

    pub fn by_kind(&self, kind: ArtifactKind) -> Vec<&Artifact> {
        self.artifacts.iter().filter(|a| a.kind == kind).collect()
    }

    pub fn by_kind_and_crate(&self, kind: ArtifactKind, crate_name: &str) -> Vec<&Artifact> {
        self.artifacts.iter()
            .filter(|a| a.kind == kind && a.crate_name == crate_name)
            .collect()
    }

    pub fn all(&self) -> &[Artifact] { &self.artifacts }
}
```

- [ ] **Step 4: Run tests, verify pass**

Run: `cd /opt/repos/anodize && cargo test -p anodize-core artifact`
Expected: All pass.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(core): artifact registry"
```

---

### Task 6: Core — Git State Detection

**Files:**
- Create: `crates/core/src/git.rs`
- Test: inline tests

- [ ] **Step 1: Write tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_semver() {
        let v = parse_semver("v1.2.3").unwrap();
        assert_eq!(v.major, 1);
        assert_eq!(v.minor, 2);
        assert_eq!(v.patch, 3);
        assert_eq!(v.prerelease, None);
    }

    #[test]
    fn test_parse_semver_prerelease() {
        let v = parse_semver("v1.0.0-rc.1").unwrap();
        assert_eq!(v.major, 1);
        assert_eq!(v.prerelease, Some("rc.1".to_string()));
    }

    #[test]
    fn test_parse_semver_with_prefix() {
        let v = parse_semver("cfgd-core-v2.1.0").unwrap();
        assert_eq!(v.major, 2);
        assert_eq!(v.minor, 1);
    }

    #[test]
    fn test_is_prerelease() {
        assert!(parse_semver("v1.0.0-rc.1").unwrap().is_prerelease());
        assert!(!parse_semver("v1.0.0").unwrap().is_prerelease());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /opt/repos/anodize && cargo test -p anodize-core git`
Expected: FAIL

- [ ] **Step 3: Implement**

Implement:
- `SemVer` struct (major, minor, patch, prerelease)
- `parse_semver(tag: &str) -> Result<SemVer>` — extracts version from tag strings
- `GitInfo` struct (tag, commit, short_commit, branch, dirty, semver)
- `detect_git_info(tag: &str) -> Result<GitInfo>` — runs git commands to populate state
- `find_latest_tag(tag_template: &str) -> Result<Option<String>>` — scans tags matching a pattern
- `get_commits_between(from: &str, to: &str, path_filter: Option<&str>) -> Result<Vec<Commit>>`
- `Commit` struct (hash, short_hash, message, author)

The git functions shell out to `git` CLI via `std::process::Command`.

- [ ] **Step 4: Run tests, verify pass**

Run: `cd /opt/repos/anodize && cargo test -p anodize-core git`
Expected: All pass (semver parsing tests pass; git CLI tests may need a temp repo fixture).

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(core): git state detection and semver parsing"
```

---

### Task 7: Core — Context and Stage Trait

**Files:**
- Create: `crates/core/src/stage.rs`
- Create: `crates/core/src/context.rs`
- Test: inline tests

- [ ] **Step 1: Write tests**

```rust
// context.rs tests
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_template_vars() {
        let config = Config { project_name: "test".to_string(), ..Default::default() };
        let ctx = Context::new(config, ContextOptions::default());
        assert_eq!(ctx.template_vars().get("ProjectName"), Some(&"test".to_string()));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /opt/repos/anodize && cargo test -p anodize-core context`
Expected: FAIL

- [ ] **Step 3: Implement Stage trait and Context**

`crates/core/src/stage.rs`:
```rust
use crate::context::Context;
use anyhow::Result;

pub trait Stage {
    fn name(&self) -> &str;
    fn run(&self, ctx: &mut Context) -> Result<()>;
}
```

`crates/core/src/context.rs`:
```rust
use crate::artifact::ArtifactRegistry;
use crate::config::Config;
use crate::git::GitInfo;
use crate::template::TemplateVars;

pub struct ContextOptions {
    pub snapshot: bool,
    pub dry_run: bool,
    pub skip_stages: Vec<String>,
    pub selected_crates: Vec<String>,
}

impl Default for ContextOptions {
    fn default() -> Self {
        Self {
            snapshot: false,
            dry_run: false,
            skip_stages: vec![],
            selected_crates: vec![],
        }
    }
}

pub struct Context {
    pub config: Config,
    pub artifacts: ArtifactRegistry,
    pub options: ContextOptions,
    template_vars: TemplateVars,
    pub git_info: Option<GitInfo>,
}

impl Context {
    pub fn new(config: Config, options: ContextOptions) -> Self {
        let mut vars = TemplateVars::new();
        vars.set("ProjectName", &config.project_name);
        // Git info, version, etc. populated later
        Self {
            config,
            artifacts: ArtifactRegistry::new(),
            options,
            template_vars: vars,
            git_info: None,
        }
    }

    pub fn template_vars(&self) -> &TemplateVars { &self.template_vars }
    pub fn template_vars_mut(&mut self) -> &mut TemplateVars { &mut self.template_vars }

    pub fn render_template(&self, template: &str) -> anyhow::Result<String> {
        crate::template::render(template, &self.template_vars)
    }

    pub fn should_skip(&self, stage_name: &str) -> bool {
        self.options.skip_stages.iter().any(|s| s == stage_name)
    }

    pub fn is_dry_run(&self) -> bool { self.options.dry_run }
    pub fn is_snapshot(&self) -> bool { self.options.snapshot }
}
```

- [ ] **Step 4: Run tests, verify pass**

Run: `cd /opt/repos/anodize && cargo test -p anodize-core`
Expected: All pass.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(core): Stage trait and Context"
```

---

### Task 8: CLI — Config Loading and Basic Commands

**Files:**
- Create: `crates/cli/src/main.rs`
- Create: `crates/cli/src/commands/mod.rs`
- Create: `crates/cli/src/commands/release.rs`
- Create: `crates/cli/src/commands/build.rs`
- Create: `crates/cli/src/commands/check.rs`
- Create: `crates/cli/src/commands/init.rs`
- Create: `crates/cli/src/commands/changelog.rs`
- Create: `crates/cli/src/pipeline.rs`

- [ ] **Step 1: Implement clap CLI structure**

```rust
// main.rs
use clap::{Parser, Subcommand};
use anyhow::Result;

mod commands;
mod pipeline;

#[derive(Parser)]
#[command(name = "anodize", version, about = "Release Rust projects with ease")]
struct Cli {
    #[arg(long, global = true)]
    verbose: bool,
    #[arg(long, global = true)]
    debug: bool,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the full release pipeline
    Release {
        #[arg(long = "crate", action = clap::ArgAction::Append)]
        crate_names: Vec<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        snapshot: bool,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        clean: bool,
        #[arg(long, value_delimiter = ',')]
        skip: Vec<String>,
        #[arg(long)]
        token: Option<String>,
    },
    /// Build binaries only
    Build {
        #[arg(long = "crate", action = clap::ArgAction::Append)]
        crate_names: Vec<String>,
    },
    /// Validate configuration
    Check,
    /// Generate starter config
    Init,
    /// Generate changelog only
    Changelog {
        #[arg(long = "crate")]
        crate_name: Option<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Release { crate_names, all, force, snapshot, dry_run, clean, skip, token } => {
            commands::release::run(ReleaseOpts {
                crate_names, all, force, snapshot, dry_run, clean, skip, token,
                verbose: cli.verbose, debug: cli.debug,
            })
        }
        Commands::Build { crate_names } => commands::build::run(crate_names),
        Commands::Check => commands::check::run(),
        Commands::Init => commands::init::run(),
        Commands::Changelog { crate_name } => commands::changelog::run(crate_name),
    }
}
```

- [ ] **Step 2: Implement config file discovery and loading**

In `pipeline.rs`, implement:
- `find_config() -> Result<PathBuf>` — searches for `anodize.yaml`, `anodize.toml`, `.anodize.yaml`, `.anodize.toml`
- `load_config(path: &Path) -> Result<Config>` — deserializes based on extension

- [ ] **Step 3: Implement stub commands**

Each command module (`release.rs`, `build.rs`, etc.) gets a `pub fn run() -> Result<()>` that loads config and prints a message. The `release` command assembles and runs the pipeline.

- [ ] **Step 4: Verify CLI compiles and runs**

Run: `cd /opt/repos/anodize && cargo run -- --help`
Expected: Shows help with release, build, check, init, changelog subcommands.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(cli): clap CLI with config loading and command stubs"
```

---

### Task 9: CLI — Pipeline Assembly and Execution

**Files:**
- Modify: `crates/cli/src/pipeline.rs`
- Modify: `crates/cli/src/commands/release.rs`

- [ ] **Step 1: Implement pipeline runner**

```rust
// pipeline.rs
use anodize_core::context::Context;
use anodize_core::stage::Stage;
use anyhow::Result;

pub struct Pipeline {
    stages: Vec<Box<dyn Stage>>,
}

impl Pipeline {
    pub fn new() -> Self { Self { stages: vec![] } }

    pub fn add(&mut self, stage: Box<dyn Stage>) {
        self.stages.push(stage);
    }

    pub fn run(&self, ctx: &mut Context) -> Result<()> {
        for stage in &self.stages {
            if ctx.should_skip(stage.name()) {
                eprintln!("  • skipping {}", stage.name());
                continue;
            }
            eprintln!("  • running {}...", stage.name());
            stage.run(ctx)?;
            eprintln!("  ✓ {}", stage.name());
        }
        Ok(())
    }
}
```

- [ ] **Step 2: Wire up release command to build pipeline with all stages**

```rust
// commands/release.rs
pub fn run(opts: ReleaseOpts) -> Result<()> {
    let config = pipeline::load_config(&pipeline::find_config()?)?;

    // --clean: remove dist/ before building
    if opts.clean {
        let dist = &config.dist;
        if dist.exists() {
            std::fs::remove_dir_all(dist)?;
        }
    }

    let ctx_opts = ContextOptions {
        snapshot: opts.snapshot,
        dry_run: opts.dry_run,
        skip_stages: opts.skip,
        selected_crates: opts.crate_names,
        token: opts.token,
    };
    let mut ctx = Context::new(config, ctx_opts);

    let mut pipeline = Pipeline::new();
    // Before hooks (shell out)
    pipeline.add(Box::new(BuildStage));
    pipeline.add(Box::new(ArchiveStage));
    pipeline.add(Box::new(NfpmStage));
    pipeline.add(Box::new(ChecksumStage));
    pipeline.add(Box::new(ChangelogStage));
    pipeline.add(Box::new(ReleaseStage));
    pipeline.add(Box::new(PublishStage));
    pipeline.add(Box::new(DockerStage));
    pipeline.add(Box::new(SignStage));
    pipeline.add(Box::new(AnnounceStage));
    // After hooks (shell out)

    pipeline.run(&mut ctx)
}
```

- [ ] **Step 3: Verify pipeline runs (stages are still todo!())**

Run: `cd /opt/repos/anodize && cargo build`
Expected: Compiles. Running would panic at first stage's `todo!()`.

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "feat(cli): pipeline assembly and execution"
```

---

## Phase 2: Build Pipeline

### Task 10: Stage — Build

**Files:**
- Modify: `crates/stage-build/src/lib.rs`
- Test: inline tests + integration test with temp Cargo project

- [ ] **Step 1: Write tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_command_for_native_cargo() {
        let cmd = build_command(
            "cfgd", "crates/cfgd", "x86_64-unknown-linux-gnu",
            &CrossStrategy::Cargo, "--release", &[], false, &Default::default(),
        );
        assert_eq!(cmd.program, "cargo");
        assert!(cmd.args.contains(&"build".to_string()));
        assert!(cmd.args.contains(&"--target".to_string()));
        assert!(cmd.args.contains(&"--release".to_string()));
    }

    #[test]
    fn test_build_command_for_zigbuild() {
        let cmd = build_command(
            "cfgd", "crates/cfgd", "aarch64-unknown-linux-gnu",
            &CrossStrategy::Zigbuild, "--release", &[], false, &Default::default(),
        );
        assert_eq!(cmd.program, "cargo");
        assert!(cmd.args.contains(&"zigbuild".to_string()));
    }

    #[test]
    fn test_build_command_for_cross() {
        let cmd = build_command(
            "cfgd", "crates/cfgd", "aarch64-unknown-linux-gnu",
            &CrossStrategy::Cross, "--release", &[], false, &Default::default(),
        );
        assert_eq!(cmd.program, "cross");
    }

    #[test]
    fn test_build_command_with_features() {
        let cmd = build_command(
            "cfgd", "crates/cfgd", "x86_64-unknown-linux-gnu",
            &CrossStrategy::Cargo, "--release",
            &["tls".to_string(), "json".to_string()], false, &Default::default(),
        );
        assert!(cmd.args.contains(&"--features".to_string()));
        assert!(cmd.args.contains(&"tls,json".to_string()));
    }

    #[test]
    fn test_detect_cross_strategy_auto() {
        // Auto detection logic — returns Cargo as fallback when nothing installed
        let strategy = detect_cross_strategy();
        // At minimum, cargo is always available
        assert!(matches!(strategy, CrossStrategy::Cargo | CrossStrategy::Zigbuild | CrossStrategy::Cross));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /opt/repos/anodize && cargo test -p anodize-stage-build`
Expected: FAIL

- [ ] **Step 3: Implement BuildStage**

Implement:
- `BuildCommand` struct (program, args, env)
- `build_command(...)` — constructs the cargo/zigbuild/cross command
- `detect_cross_strategy()` — checks which tools are available (via `which`)
- `BuildStage::run()` — iterates crates × targets, runs builds, handles `copy_from`, registers `Binary` artifacts
- Per-target env var injection from config

- [ ] **Step 4: Run tests, verify pass**

Run: `cd /opt/repos/anodize && cargo test -p anodize-stage-build`
Expected: All pass.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(stage-build): cargo/zigbuild/cross build orchestration"
```

---

### Task 11: Stage — Archive

**Files:**
- Modify: `crates/stage-archive/src/lib.rs`
- Test: inline tests with temp dirs

- [ ] **Step 1: Write tests**

Test creating tar.gz and zip archives from dummy binary files in a temp directory. Verify the archive contains the expected files and the artifact is registered.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /opt/repos/anodize && cargo test -p anodize-stage-archive`

- [ ] **Step 3: Implement ArchiveStage**

Add `flate2`, `tar`, `zip` to workspace dependencies. Implement:
- `create_tar_gz(files, output_path)` — creates a tar.gz archive
- `create_zip(files, output_path)` — creates a zip archive
- `ArchiveStage::run()` — for each crate, queries `Binary` artifacts, groups by target, creates archives per archive config, handles `format_overrides` by OS, handles `binaries` filter (include all if omitted), registers `Archive` artifacts

- [ ] **Step 4: Run tests, verify pass**

Run: `cd /opt/repos/anodize && cargo test -p anodize-stage-archive`

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(stage-archive): tar.gz and zip archive creation"
```

---

### Task 12: Stage — NFpm

**Files:**
- Modify: `crates/stage-nfpm/src/lib.rs`
- Test: inline tests

- [ ] **Step 1: Write tests**

Test that the nFPM config YAML is generated correctly from the crate config. Test that the stage is skipped when no `nfpm` block is present.

- [ ] **Step 2: Implement NfpmStage**

Shells out to `nfpm pkg` CLI. Generates a temporary nfpm config YAML from the anodize config, invokes nfpm for each format (.deb, .rpm, .apk), registers `LinuxPackage` artifacts. Skipped if crate has no `nfpm` config.

- [ ] **Step 3: Run tests, verify pass**

Run: `cd /opt/repos/anodize && cargo test -p anodize-stage-nfpm`

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "feat(stage-nfpm): Linux package generation"
```

---

### Task 13: Stage — Checksum

**Files:**
- Modify: `crates/stage-checksum/src/lib.rs`
- Test: inline tests

- [ ] **Step 1: Write tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_sha256_file() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"hello world").unwrap();
        let hash = sha256_file(f.path()).unwrap();
        assert_eq!(hash, "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9");
    }

    #[test]
    fn test_checksums_line_format() {
        let line = format_checksum_line("abcdef1234", "myfile.tar.gz");
        assert_eq!(line, "abcdef1234  myfile.tar.gz");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /opt/repos/anodize && cargo test -p anodize-stage-checksum`

- [ ] **Step 3: Implement ChecksumStage**

Add `sha2` to workspace dependencies. Implement:
- `sha256_file(path) -> Result<String>` — computes SHA256
- `sha512_file(path) -> Result<String>` — computes SHA512
- `hash_file(path, algorithm) -> Result<String>` — dispatches to sha256 or sha512 based on config
- `ChecksumStage::run()` — queries `Archive` and `LinuxPackage` artifacts, computes checksums using the configured algorithm, writes combined checksums file, writes per-file `.sha256`/`.sha512` files, registers `Checksum` artifacts. Skipped for crates with `archives: false` and no nfpm.

- [ ] **Step 4: Run tests, verify pass**

Run: `cd /opt/repos/anodize && cargo test -p anodize-stage-checksum`

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(stage-checksum): SHA256 checksum generation"
```

---

## Phase 3: Release Pipeline

### Task 14: Stage — Changelog

**Files:**
- Modify: `crates/stage-changelog/src/lib.rs`
- Test: inline tests

- [ ] **Step 1: Write tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_conventional_commit() {
        let commit = parse_commit_message("feat: add new feature");
        assert_eq!(commit.kind, "feat");
        assert_eq!(commit.description, "add new feature");
    }

    #[test]
    fn test_group_commits() {
        let commits = vec![
            CommitInfo { kind: "feat".into(), description: "new thing".into(), hash: "abc".into() },
            CommitInfo { kind: "fix".into(), description: "broken thing".into(), hash: "def".into() },
            CommitInfo { kind: "feat".into(), description: "another thing".into(), hash: "ghi".into() },
        ];
        let groups = vec![
            ChangelogGroup { title: "Features".into(), regexp: "^feat".into(), order: 0 },
            ChangelogGroup { title: "Bug Fixes".into(), regexp: "^fix".into(), order: 1 },
        ];
        let result = group_commits(&commits, &groups);
        assert_eq!(result[0].title, "Features");
        assert_eq!(result[0].commits.len(), 2);
        assert_eq!(result[1].title, "Bug Fixes");
        assert_eq!(result[1].commits.len(), 1);
    }

    #[test]
    fn test_render_changelog() {
        let grouped = vec![
            GroupedCommits {
                title: "Features".into(),
                commits: vec![
                    CommitInfo { kind: "feat".into(), description: "add X".into(), hash: "abc1234".into() },
                ],
            },
        ];
        let md = render_changelog(&grouped);
        assert!(md.contains("## Features"));
        assert!(md.contains("add X"));
    }

    #[test]
    fn test_exclude_filter() {
        // apply_filters matches against the raw commit message, not parsed fields
        let commits = vec![
            CommitInfo { raw_message: "docs: update readme".into(), kind: "docs".into(), description: "update readme".into(), hash: "a".into() },
            CommitInfo { raw_message: "feat: new feature".into(), kind: "feat".into(), description: "new feature".into(), hash: "b".into() },
        ];
        let filters = vec!["^docs:".to_string()];
        let filtered = apply_filters(&commits, &filters);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].kind, "feat");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd /opt/repos/anodize && cargo test -p anodize-stage-changelog`

- [ ] **Step 3: Implement ChangelogStage**

Implement:
- `parse_commit_message(msg) -> CommitInfo` — parses conventional commit format
- `apply_filters(commits, exclude_patterns) -> Vec<CommitInfo>` — regex-based filtering
- `group_commits(commits, groups) -> Vec<GroupedCommits>` — groups by regex match
- `render_changelog(grouped) -> String` — renders markdown
- `ChangelogStage::run()` — uses `git::get_commits_between()` with path scoping per-crate, applies changelog config (filters, groups, sort), writes `RELEASE_NOTES.md` to dist, stores in context for release stage

- [ ] **Step 4: Run tests, verify pass**

Run: `cd /opt/repos/anodize && cargo test -p anodize-stage-changelog`

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(stage-changelog): conventional commit changelog generation"
```

---

### Task 15: Stage — Release

**Files:**
- Modify: `crates/stage-release/src/lib.rs`
- Test: inline tests (mock-based or integration with dry_run)

- [ ] **Step 1: Write tests**

Test the release payload construction (tag, name, body, draft, prerelease detection). Use a mock or dry-run mode to avoid hitting GitHub API.

- [ ] **Step 2: Implement ReleaseStage**

Add `octocrab` and `tokio` to workspace dependencies. Implement:
- `ReleaseStage::run()` — for each crate with a `release` block: creates GitHub Release via octocrab, uploads `Archive` + `Checksum` + `LinuxPackage` artifacts as release assets, attaches changelog as body. Handles `prerelease: auto` by checking tag for `-rc`, `-beta`, `-alpha`. Skipped in dry-run mode (logs what would happen). Token from `GITHUB_TOKEN` env var.

- [ ] **Step 3: Run tests, verify pass**

Run: `cd /opt/repos/anodize && cargo test -p anodize-stage-release`

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "feat(stage-release): GitHub Release creation via octocrab"
```

---

### Task 16: Stage — Publish (crates.io)

**Files:**
- Create: `crates/stage-publish/src/crates_io.rs`
- Modify: `crates/stage-publish/src/lib.rs`
- Test: inline tests

- [ ] **Step 1: Write tests**

Test dependency ordering (topological sort of `depends_on`), command construction for `cargo publish`, and index polling logic.

- [ ] **Step 2: Implement crates.io publishing**

Implement:
- Topological sort of crates by `depends_on`
- Shell out to `cargo publish -p <crate>` for each
- After publishing a dependency, poll crates.io sparse index (`https://index.crates.io`) to confirm the version appears. Retry with exponential backoff (5s, 10s, 20s, 40s, 60s×5). Configurable timeout.
- Skipped in dry-run mode.

- [ ] **Step 3: Run tests, verify pass**

Run: `cd /opt/repos/anodize && cargo test -p anodize-stage-publish`

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "feat(stage-publish): crates.io publishing with dependency ordering"
```

---

### Task 17: Stage — Publish (Homebrew)

**Files:**
- Create: `crates/stage-publish/src/homebrew.rs`
- Modify: `crates/stage-publish/src/lib.rs`
- Test: inline tests

- [ ] **Step 1: Write tests**

Test formula generation — given archive URLs, checksums, and config, produce the correct Ruby formula string.

- [ ] **Step 2: Implement Homebrew publishing**

Implement:
- `generate_formula(config, archives, checksums) -> String` — renders Homebrew Ruby formula
- Clone tap repo, write formula to `Formula/<name>.rb`, commit, push
- Token via `HOMEBREW_TAP_TOKEN` or `GITHUB_TOKEN`
- Skipped in dry-run mode.

- [ ] **Step 3: Run tests, verify pass**

Run: `cd /opt/repos/anodize && cargo test -p anodize-stage-publish homebrew`

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "feat(stage-publish): Homebrew tap formula generation"
```

---

### Task 18: Stage — Publish (Scoop)

**Files:**
- Create: `crates/stage-publish/src/scoop.rs`
- Modify: `crates/stage-publish/src/lib.rs`
- Test: inline tests

- [ ] **Step 1: Write tests**

Test Scoop manifest JSON generation.

- [ ] **Step 2: Implement Scoop publishing**

Similar to Homebrew: generate JSON manifest, clone bucket repo, write manifest, commit, push. Skipped in dry-run mode.

- [ ] **Step 3: Run tests, verify pass**

Run: `cd /opt/repos/anodize && cargo test -p anodize-stage-publish scoop`

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "feat(stage-publish): Scoop bucket manifest generation"
```

---

### Task 19: Stage — Docker

**Files:**
- Modify: `crates/stage-docker/src/lib.rs`
- Test: inline tests

- [ ] **Step 1: Write tests**

Test staging directory creation (binary placement into `binaries/amd64/`, `binaries/arm64/`), Dockerfile copy, and `docker buildx build` command construction.

- [ ] **Step 2: Implement DockerStage**

Implement:
- Create staging directory: `dist/docker/<crate>/<image-index>/binaries/<arch>/<binary>`
- Copy Dockerfile into staging directory
- Construct `docker buildx build` command with `--platform`, `--push`, `--tag` flags
- Multiple image definitions per crate, multiple tags per image
- Register `DockerImage` artifacts
- Skipped in dry-run mode.

- [ ] **Step 3: Run tests, verify pass**

Run: `cd /opt/repos/anodize && cargo test -p anodize-stage-docker`

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "feat(stage-docker): multi-arch Docker image builds via buildx"
```

---

### Task 20: Stage — Sign

**Files:**
- Modify: `crates/stage-sign/src/lib.rs`
- Test: inline tests

- [ ] **Step 1: Write tests**

Test command construction for GPG and cosign, template variable resolution for `{{ .Signature }}` and `{{ .Artifact }}`.

- [ ] **Step 2: Implement SignStage**

Implement:
- For `sign` config: iterate artifacts matching `artifacts` filter (checksum/all/none), shell out to configured `cmd` with `args`, resolving `{{ .Signature }}` and `{{ .Artifact }}` templates
- For `docker_signs` config: iterate `DockerImage` artifacts, shell out to cosign
- Skipped in dry-run mode.

- [ ] **Step 3: Run tests, verify pass**

Run: `cd /opt/repos/anodize && cargo test -p anodize-stage-sign`

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "feat(stage-sign): GPG and cosign signing"
```

---

### Task 21: Stage — Announce

**Files:**
- Create: `crates/stage-announce/src/discord.rs`
- Create: `crates/stage-announce/src/slack.rs`
- Create: `crates/stage-announce/src/webhook.rs`
- Modify: `crates/stage-announce/src/lib.rs`
- Test: inline tests

- [ ] **Step 1: Write tests**

Test webhook payload construction for Discord, Slack, and generic HTTP webhook. Test that disabled providers are skipped.

- [ ] **Step 2: Implement AnnounceStage**

Add `reqwest` to workspace dependencies. Implement:
- `send_discord(webhook_url, message)` — POST to Discord webhook
- `send_slack(webhook_url, message)` — POST to Slack webhook
- `send_webhook(config)` — POST to generic HTTP endpoint with custom headers
- `AnnounceStage::run()` — check each provider's `enabled` flag, render `message_template`, send. Skipped in dry-run mode.

- [ ] **Step 3: Run tests, verify pass**

Run: `cd /opt/repos/anodize && cargo test -p anodize-stage-announce`

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "feat(stage-announce): Discord, Slack, and webhook announce"
```

---

## Phase 4: CLI Polish

### Task 22: `init` Command

**Files:**
- Modify: `crates/cli/src/commands/init.rs`
- Test: integration test with temp Cargo workspace

- [ ] **Step 1: Write tests**

Create a temp directory with a Cargo workspace containing two crates (one binary, one library). Run `init` logic and verify the generated config has correct crate entries, dependency ordering, and target triples.

- [ ] **Step 2: Implement init command**

Implement:
- Parse `Cargo.toml` workspace members
- Discover which crates have `[[bin]]` targets vs library-only
- Build dependency graph from `[dependencies]` sections
- Generate `anodize.yaml` with sensible defaults (common target triples, archive config, release config)
- Write to stdout or file

- [ ] **Step 3: Run tests, verify pass**

Run: `cd /opt/repos/anodize && cargo test -p anodize init`

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "feat(cli): init command generates config from Cargo workspace"
```

---

### Task 23: `check` Command

**Files:**
- Modify: `crates/cli/src/commands/check.rs`
- Test: inline tests

- [ ] **Step 1: Write tests**

Test schema validation (missing required fields), semantic validation (cyclic `depends_on`, unknown target triples, missing `{{ .Version }}` in tag_template), and environment checks (mock tool availability).

- [ ] **Step 2: Implement check command**

Implement three validation levels:
1. Schema validation — handled by serde deserialize errors
2. Semantic validation — check `depends_on` DAG, validate target triples, verify `path` directories exist, `tag_template` contains `{{ .Version }}`, `copy_from` references valid binary
3. Environment checks — `which` for cargo-zigbuild, cross, docker, gpg, cosign; check `GITHUB_TOKEN` env var

Print errors and warnings with colors. Exit non-zero on errors.

- [ ] **Step 3: Run tests, verify pass**

Run: `cd /opt/repos/anodize && cargo test -p anodize check`

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "feat(cli): check command with schema, semantic, and env validation"
```

---

### Task 24: Change Detection (`--all`)

**Files:**
- Modify: `crates/core/src/git.rs`
- Modify: `crates/cli/src/commands/release.rs`
- Test: integration test with temp git repo

- [ ] **Step 1: Write tests**

Create a temp git repo with tagged commits, make changes in a sub-path, verify that change detection correctly identifies which crates have unreleased changes.

- [ ] **Step 2: Implement change detection**

In `core/git.rs`:
- `find_latest_tag_matching(pattern: &str) -> Result<Option<String>>` — git tag --list, regex match, semver sort
- `has_changes_since(tag: &str, path: &str) -> Result<bool>` — git diff --name-only filtered to path
- `workspace_files_changed(tag: &str) -> Result<bool>` — checks Cargo.toml, Cargo.lock

In `release.rs`:
- When `--all` is passed, iterate crates, check for changes, filter to those with changes (unless `--force`), topologically sort by `depends_on`

- [ ] **Step 3: Run tests, verify pass**

Run: `cd /opt/repos/anodize && cargo test -p anodize-core git::tests::test_change_detection`

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "feat: --all change detection with path-scoped tag matching"
```

---

### Task 25: Hooks (Before/After)

**Files:**
- Modify: `crates/cli/src/pipeline.rs`
- Test: inline tests

- [ ] **Step 1: Write tests**

Test that before/after hooks execute shell commands in order and fail the pipeline if a hook fails.

- [ ] **Step 2: Implement hooks**

In `pipeline.rs`, before running stages, iterate `config.before.hooks` and execute each via `std::process::Command`. Same for `config.after.hooks` after stages complete. Propagate non-zero exit codes as errors. Skip in dry-run mode (log only).

- [ ] **Step 3: Run tests, verify pass**

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "feat(cli): before/after hook execution"
```

---

### Task 26: UX — Colored Output and Progress

**Files:**
- Modify: `crates/cli/src/pipeline.rs`
- Modify: `crates/cli/src/main.rs`

- [ ] **Step 1: Add colored output**

Add `colored` to workspace dependencies. Update pipeline runner to use colored stage names, checkmarks for success, X marks for failure. Add `--verbose` / `--debug` flags to CLI. In debug mode, print full command invocations. Format errors with suggestions (e.g., missing tools).

- [ ] **Step 2: Verify output looks good**

Run: `cd /opt/repos/anodize && cargo run -- check` (with a test config)
Expected: Colored, formatted output.

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "feat(cli): colored output with stage progress indicators"
```

---

## Phase 5: Integration

### Task 27: Integration Tests

**Files:**
- Create: `tests/integration/mod.rs`
- Create: `tests/integration/snapshot.rs`

- [ ] **Step 1: Write end-to-end snapshot test**

Create a temp Cargo workspace with a simple binary crate, write an `anodize.yaml`, run `anodize release --snapshot --skip=publish,docker,announce,sign`, verify:
- Binary built for host target
- Archive created in `dist/`
- Checksums file created
- Changelog generated
- No GitHub release created (snapshot mode)

- [ ] **Step 2: Write config validation integration test**

Create valid and invalid configs, run `anodize check`, verify exit codes and error messages.

- [ ] **Step 3: Write init integration test**

Create a Cargo workspace, run `anodize init`, verify generated config.

- [ ] **Step 4: Run all tests**

Run: `cd /opt/repos/anodize && cargo test`
Expected: All pass.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "test: integration tests for snapshot release, check, and init"
```

---

### Task 28: README

**Files:**
- Create: `README.md`

- [ ] **Step 1: Write README**

Include:
- Project description (Rust GoReleaser alternative)
- Installation (`cargo install anodize`)
- Quick start (`anodize init`, `anodize check`, `anodize release`)
- Config example (minimal)
- CLI reference
- Comparison with GoReleaser
- License

- [ ] **Step 2: Commit**

```bash
git add README.md && git commit -m "docs: add README"
```

---

## Task Dependency Graph

```
Task 1 (scaffolding)
├── Task 2 (config schema)      ─┐
├── Task 3 (target mapping)      │
├── Task 4 (template engine)     ├─→ Task 7 (context + stage trait)
├── Task 5 (artifact registry)   │       └── Task 8 (CLI config loading)
└── Task 6 (git state)          ─┘           └── Task 9 (pipeline assembly)
                                                  ├── Task 10 (build stage)
                                                  │   ├── Task 11 (archive stage) ─┐
                                                  │   ├── Task 12 (nfpm stage)     ├─→ Task 13 (checksum)
                                                  │   └── Task 19 (docker stage)   ┘
                                                  ├── Task 14 (changelog stage)  ─┐
                                                  ├── Task 15 (release stage)  ←──┘ (uses changelog at runtime)
                                                  ├── Task 16-18 (publish stages)
                                                  ├── Task 20 (sign stage)
                                                  ├── Task 21 (announce stage)
                                                  └── Task 25 (hooks)

Task 22 (init) — depends on Task 2
Task 23 (check) — depends on Task 2
Task 24 (change detection) — depends on Task 6
Task 26 (UX) — depends on Task 9
Task 27 (integration tests) — depends on all stages
Task 28 (README) — last
```

**Parallelization notes:**
- Tasks 2-6 can be parallelized (all in core, no inter-dependencies).
- Task 7 depends on Tasks 2, 4, 5, and 6 (Context uses Config, TemplateVars, ArtifactRegistry, GitInfo).
- Tasks 11, 12, 13 form a chain after Task 10 (archive/nfpm need binaries, checksum needs archives).
- Task 19 (Docker) depends on Task 10 (needs built binaries).
- Tasks 14, 15, 16-18, 20, 21 can be parallelized with each other after core is done.
- Tasks 14 and 15 can be *implemented* independently, but the pipeline must order changelog before release (already done in Task 9).
