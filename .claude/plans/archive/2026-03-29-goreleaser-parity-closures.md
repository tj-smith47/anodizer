> **STATUS (archived 2026-05-06):** All 18 tasks accounted for — 16
> DONE in code, Task 15 (`if` on global hooks) reclassified N/A
> (GoReleaser itself lacks the field; see `parity-session-index.md:136`),
> Task 18 superseded by `cc098a9` which deleted the target
> `goreleaser-parity-matrix.md` in favour of
> `goreleaser-complete-feature-inventory.md` + `parity-session-index.md`.
> Canonical tracker: `parity-session-index.md` (444/444). Per-checkbox
> reconciliation skipped.

# GoReleaser Parity Closures Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close every Missing and Partial item in `.claude/specs/goreleaser-parity-matrix.md` so anodizer achieves full GoReleaser behavioral parity.

**Architecture:** Each task targets one subsystem (stage). Config field additions in `crates/core/src/config.rs` are paired with their stage wiring in the same task so context stays together. Items genuinely N/A for a Rust tool (gomod proxy, ModulePath, goamd64) are re-marked as Implemented/N/A rather than implemented.

**Tech Stack:** Rust, Tera templates, serde, clap, octocrab (GitHub API), reqwest (HTTP)

**Pre-completed:** Task 1 (template engine) was already implemented prior to planning.

---

### Task 1: Template Engine — All Missing Functions/Filters [DONE]

**Files:**
- Modified: `crates/core/Cargo.toml` (added hash deps)
- Modified: `crates/core/src/template.rs` (added functions+filters)
- Modified: `crates/core/src/context.rs` (added Runtime nested vars)

Already implemented:
- [x] `incpatch`/`incminor`/`incmajor` version increment functions
- [x] 14 hash functions (sha1, sha224, sha256, sha384, sha512, sha3_224, sha3_256, sha3_384, sha3_512, blake2b, blake2s, blake3, md5, crc32)
- [x] `readFile`/`mustReadFile` file reading
- [x] `time(format=)` current UTC time
- [x] `dir`/`base`/`abs` path filters
- [x] `urlPathEscape` filter
- [x] `mdv2escape` MarkdownV2 escape filter
- [x] `contains(s=, substr=)` string containment function
- [x] `list`/`englishJoin` array creation and joining
- [x] `filter`/`reverseFilter` regex array filtering
- [x] `indexOrDefault` map lookup with default
- [x] `Runtime.Goos`/`Runtime.Goarch` nested object in Tera context
- [x] Verified compilation, all existing tests pass

**Remaining template items (Go-style positional syntax):** The Go-style `replace "old" "new"` and `split "sep"` positional call syntax is not supported. Tera's keyword-arg equivalents (`| replace(from="old", to="new")` and `| split(pat=".")`) work. This is a syntax difference, not a missing feature — mark as Implemented in parity matrix with a note about syntax.

---

### Task 2: Build Stage — Config + Wiring

**Files:**
- Modify: `crates/core/src/config.rs` — `BuildConfig`, `CrateConfig`, `Defaults` structs
- Modify: `crates/stage-build/src/lib.rs` — wire new fields

**Parity items addressed:**
- `builds[].binary` template expansion (Partial → Implemented)
- `builds[].targets` default list (Partial → Implemented)
- `builds[].env` template expansion on values (Partial → Implemented)
- `builds[].reproducible` set binary file mtime (Partial → Implemented)
- `builds[].hooks.pre/post` per-build hooks (Missing → Implemented)
- Cross-compilation `tool` arbitrary binary (Partial → Implemented)
- `defaults.ignore` per-build (Partial → Implemented)
- `defaults.overrides` per-build (Partial → Implemented)
- `no_unique_dist_dir` (Missing → Implemented)
- `gomod proxy` (Missing → N/A for Rust)
- `no_main_check` (Missing → N/A for Rust)

- [ ] **Step 1: Add config fields to `BuildConfig`**

In `crates/core/src/config.rs`, add to `BuildConfig` struct:

```rust
pub struct BuildConfig {
    // ... existing fields ...
    /// Pre/post build hooks.
    pub hooks: Option<BuildHooksConfig>,
    /// Per-build ignore rules (overrides defaults.ignore).
    pub ignore: Option<Vec<BuildIgnore>>,
    /// Per-build overrides (overrides defaults.overrides).
    pub overrides: Option<Vec<BuildOverride>>,
}
```

Add new struct:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct BuildHooksConfig {
    pub pre: Option<Vec<HookEntry>>,
    pub post: Option<Vec<HookEntry>>,
}
```

Add to `CrateConfig`:

```rust
pub struct CrateConfig {
    // ... existing fields ...
    /// When true, all binaries share a single dist directory instead of per-target dirs.
    pub no_unique_dist_dir: Option<bool>,
}
```

Add to `CrossStrategy` enum a new variant:

```rust
pub enum CrossStrategy {
    Auto,
    Zigbuild,
    Cross,
    Cargo,
    /// Arbitrary tool binary path (e.g. "/usr/bin/my-cross-compiler").
    #[serde(untagged)]
    Tool(String),
}
```

- [ ] **Step 2: Wire `binary` template expansion in build stage**

In `crates/stage-build/src/lib.rs`, wherever `build.binary` is used to determine the output binary name, wrap it with `ctx.render_template(&build.binary)`.

- [ ] **Step 3: Wire default targets**

Add a const `DEFAULT_TARGETS` array with the 5 standard Rust targets:
```rust
const DEFAULT_TARGETS: &[&str] = &[
    "x86_64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "aarch64-unknown-linux-gnu",
];
```
When `build.targets` is None and `defaults.targets` is None, use `DEFAULT_TARGETS`.

- [ ] **Step 4: Wire env template expansion**

In the build stage where env vars are applied per-target, render each env value through `ctx.render_template()`.

- [ ] **Step 5: Wire reproducible mtime**

After binary compilation, when `build.reproducible == Some(true)`, set the output binary's file modification time to `SOURCE_DATE_EPOCH` using `filetime::set_file_mtime()`. Add `filetime = "0.2"` to `crates/stage-build/Cargo.toml` and workspace `Cargo.toml`.

- [ ] **Step 6: Wire per-build hooks**

Before building each build config, run `hooks.pre` if set. After building, run `hooks.post`. Use the existing `run_hooks()` from pipeline.rs.

- [ ] **Step 7: Wire per-build ignore/overrides**

When resolving the build matrix, check `build.ignore` first (if Some), falling back to `defaults.ignore`. Same for overrides.

- [ ] **Step 8: Wire `no_unique_dist_dir`**

When `crate_cfg.no_unique_dist_dir == Some(true)`, place all build outputs in `dist/` directly instead of `dist/{target}/`.

- [ ] **Step 9: Wire `CrossStrategy::Tool`**

In `build_command()`, handle the `Tool(binary)` variant by using the specified binary instead of `cargo`/`cross`/`cargo-zigbuild`.

- [ ] **Step 10: Compile and test**

Run: `cargo test -p anodizer-stage-build`

- [ ] **Step 11: Commit**

---

### Task 3: Archive Stage — Config + Wiring

**Files:**
- Modify: `crates/core/src/config.rs` — `ArchiveConfig`, `FormatOverride`
- Modify: `crates/stage-archive/src/lib.rs` — wire new fields

**Parity items addressed:**
- `format_overrides[].formats` plural (Missing → Implemented)
- `files` objects with src/dst/info (Missing → Implemented)
- `meta` no-binary archives (Missing → Implemented)
- `builds_info` permissions (Missing → Implemented)
- `strip_binary_directory` (Missing → Implemented)
- `allow_different_binary_count` (Missing → Implemented)
- `hooks` (Missing → Implemented)
- `gz` format (Missing → Implemented)

- [ ] **Step 1: Add config fields**

In `crates/core/src/config.rs`:

Extend `FormatOverride`:
```rust
pub struct FormatOverride {
    pub os: String,
    pub format: Option<String>,
    /// Plural format overrides (v2.6+). Takes priority over singular `format`.
    pub formats: Option<Vec<String>>,
}
```

Add `ArchiveFileEntry` struct for object-form files:
```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ArchiveFileSpec {
    Glob(String),
    Detailed {
        src: String,
        dst: Option<String>,
        info: Option<ArchiveFileInfo>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ArchiveFileInfo {
    pub owner: Option<String>,
    pub group: Option<String>,
    pub mode: Option<String>,
    pub mtime: Option<String>,
}
```

Change `ArchiveConfig.files` from `Option<Vec<String>>` to `Option<Vec<ArchiveFileSpec>>`.

Add to `ArchiveConfig`:
```rust
pub struct ArchiveConfig {
    // ... existing fields ...
    /// When true, create an archive with no binaries (metadata-only).
    pub meta: Option<bool>,
    /// File ownership/permissions applied to binaries in the archive.
    pub builds_info: Option<ArchiveFileInfo>,
    /// Strip the binary's parent directory structure in the archive.
    pub strip_binary_directory: Option<bool>,
    /// Allow archives to contain different numbers of binaries per target.
    pub allow_different_binary_count: Option<bool>,
    /// Pre/post archive hooks.
    pub hooks: Option<BuildHooksConfig>,
}
```

- [ ] **Step 2: Wire gz format**

In the archive stage format matching, add `"gz"` as a recognized format. Implement `create_gz()` that compresses using `flate2::write::GzEncoder` without tar wrapping.

- [ ] **Step 3: Wire format_overrides plural**

When resolving format for a target, check `override.formats` first (if Some and non-empty), then fall back to `override.format`.

- [ ] **Step 4: Wire ArchiveFileSpec**

In the archive stage's file inclusion logic, handle both `Glob(pattern)` strings and `Detailed { src, dst, info }` objects. For detailed entries, copy `src` to `dst` path in the archive, applying `info` permissions.

- [ ] **Step 5: Wire meta archives**

When `archive.meta == Some(true)`, skip binary inclusion entirely — only include `files` entries.

- [ ] **Step 6: Wire builds_info, strip_binary_directory, allow_different_binary_count**

Apply `builds_info` permissions to binary entries in tar/zip archives. When `strip_binary_directory` is true, place binaries at archive root. When `allow_different_binary_count` is false (default), validate binary counts match across targets.

- [ ] **Step 7: Wire archive hooks**

Run `hooks.pre` before creating each archive, `hooks.post` after.

- [ ] **Step 8: Compile and test**

Run: `cargo test -p anodizer-stage-archive`

- [ ] **Step 9: Commit**

---

### Task 4: Checksum Stage — Config + Wiring

**Files:**
- Modify: `crates/core/src/config.rs` — `ChecksumConfig`
- Modify: `crates/stage-checksum/src/lib.rs` — fix behaviors

**Parity items addressed:**
- `disable` template-conditional (Partial → Implemented)
- `extra_files` object form with name_template (Partial → Implemented)
- Sidecar behavior (Partial → Implemented)
- `name_template` with `split: true` (Partial → Implemented)

- [ ] **Step 1: Change ChecksumConfig.disable from bool to StringOrBool**

```rust
pub struct ChecksumConfig {
    // ...
    /// Disable checksums. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub disable: Option<StringOrBool>,
    /// Extra files to include. Accepts strings (globs) or objects with name_template.
    pub extra_files: Option<Vec<ExtraFileSpec>>,
    // ...
}
```

Add:
```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ExtraFileSpec {
    Glob(String),
    Detailed {
        glob: String,
        name_template: Option<String>,
    },
}
```

- [ ] **Step 2: Wire template-conditional disable**

In the checksum stage, render `disable` through the template engine if it's a `StringOrBool::String`, then check if result is `"true"`.

- [ ] **Step 3: Wire extra_files object form**

Handle both `Glob(pattern)` and `Detailed { glob, name_template }`. When `name_template` is set, use it to rename the matched file in the checksum output.

- [ ] **Step 4: Fix sidecar behavior**

Only create per-artifact sidecar `.sha256` files when `split: true`. In non-split mode (default), only write the combined checksums file.

- [ ] **Step 5: Wire name_template in split mode**

When `split: true`, use `name_template` to generate sidecar file names instead of hardcoded `{artifact}.{algorithm}`.

- [ ] **Step 6: Compile and test**

Run: `cargo test -p anodizer-stage-checksum`

- [ ] **Step 7: Commit**

---

### Task 5: Release Stage — Config + Wiring

**Files:**
- Modify: `crates/core/src/config.rs` — `ReleaseConfig`
- Modify: `crates/stage-release/src/lib.rs` — wire fields to GitHub API

**Parity items addressed:**
- `header`/`footer` from_url/from_file (Partial → Implemented)
- `extra_files` object form (Partial → Implemented)
- `mode` API wiring (Partial → Implemented)
- `target_commitish` (Missing → Implemented)
- `discussion_category_name` (Missing → Implemented)
- `include_meta` (Missing → Implemented)
- `use_existing_draft` (Missing → Implemented)
- GitLab/Gitea support (Missing → Stub with clear error)

- [ ] **Step 1: Add config fields to ReleaseConfig**

```rust
pub struct ReleaseConfig {
    // ... existing fields ...
    /// Header content. String, or object with `from_url` or `from_file`.
    #[serde(deserialize_with = "deserialize_content_source_opt", default)]
    pub header: Option<ContentSource>,
    /// Footer content. Same format as header.
    #[serde(deserialize_with = "deserialize_content_source_opt", default)]
    pub footer: Option<ContentSource>,
    /// Extra files with optional name_template.
    pub extra_files: Option<Vec<ExtraFileSpec>>,
    /// Target commitish (branch/SHA) for the release tag.
    pub target_commitish: Option<String>,
    /// GitHub Discussion category to create for this release.
    pub discussion_category_name: Option<String>,
    /// Include metadata.json and artifacts.json in the release.
    pub include_meta: Option<bool>,
    /// Re-use an existing draft release instead of creating a new one.
    pub use_existing_draft: Option<bool>,
}
```

Add new types:
```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ContentSource {
    Inline(String),
    FromFile { from_file: String },
    FromUrl { from_url: String },
}

impl ContentSource {
    pub fn resolve(&self) -> anyhow::Result<String> {
        match self {
            Self::Inline(s) => Ok(s.clone()),
            Self::FromFile { from_file } => std::fs::read_to_string(from_file)
                .map_err(|e| anyhow::anyhow!("failed to read {}: {}", from_file, e)),
            Self::FromUrl { from_url } => {
                reqwest::blocking::get(from_url)
                    .and_then(|r| r.text())
                    .map_err(|e| anyhow::anyhow!("failed to fetch {}: {}", from_url, e))
            }
        }
    }
}
```

- [ ] **Step 2: Wire header/footer from_url/from_file**

In the release stage's `build_release_body()`, call `header.resolve()` and `footer.resolve()` to get the content, then render through template engine.

- [ ] **Step 3: Wire release mode to GitHub API**

In the release creation path:
- `"keep-existing"`: If release body already exists, don't overwrite it
- `"append"`: Append new content after existing body
- `"prepend"`: Prepend new content before existing body
- `"replace"` (default for anodizer, change default to `"keep-existing"`): Replace entirely

- [ ] **Step 4: Wire target_commitish**

Pass `target_commitish` to the octocrab release creation API call.

- [ ] **Step 5: Wire discussion_category_name**

Pass to GitHub API. Requires custom JSON body field since octocrab may not expose it directly.

- [ ] **Step 6: Wire include_meta**

When `include_meta` is true, upload `metadata.json` and `artifacts.json` from dist/ as release assets.

- [ ] **Step 7: Wire use_existing_draft**

When `use_existing_draft` is true, search for existing draft releases matching the tag. If found, update it instead of creating a new one.

- [ ] **Step 8: Add reqwest dependency to stage-release if not present**

For `ContentSource::FromUrl`.

- [ ] **Step 9: Compile and test**

Run: `cargo test -p anodizer-stage-release`

- [ ] **Step 10: Commit**

---

### Task 6: Changelog Stage — Config + Wiring

**Files:**
- Modify: `crates/core/src/config.rs` — `ChangelogConfig`
- Modify: `crates/stage-changelog/src/lib.rs` — wire fields

**Parity items addressed:**
- `disable` template-conditional (Partial → Implemented)
- `abbrev` support -1 (Partial → Implemented)
- `changelog.use: github` OSS backend (Missing → Implemented)
- `changelog.format` Logins variable (Partial → Implemented when github backend used)
- Nested subgroups (Missing → Implemented)
- `changelog.use: gitlab`/`gitea` (Missing → Stub)
- AI enhancement (Missing → Stub)

- [ ] **Step 1: Change ChangelogConfig fields**

```rust
pub struct ChangelogConfig {
    // ...
    /// Disable changelog. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub disable: Option<StringOrBool>,
    /// Hash abbreviation length. -1 omits hash entirely. Default: 7.
    pub abbrev: Option<i32>,
    // ...
}
```

Add subgroups to `ChangelogGroup`:
```rust
pub struct ChangelogGroup {
    pub title: String,
    pub regexp: Option<String>,
    pub order: Option<i32>,
    /// Nested subgroups within this group (Pro feature, free in anodizer).
    pub groups: Option<Vec<ChangelogGroup>>,
}
```

- [ ] **Step 2: Wire template-conditional disable**

Render `disable` through template engine, check if result is `"true"`.

- [ ] **Step 3: Wire abbrev -1**

When `abbrev == Some(-1)`, omit the commit hash entirely from changelog entries.

- [ ] **Step 4: Implement `github` changelog backend**

When `use: github`, use the GitHub API to fetch commits between tags and resolve commit authors to GitHub usernames. Populate a `Logins` template variable with the list of usernames.

- [ ] **Step 5: Implement nested subgroups**

When rendering changelog groups, recursively process `group.groups` as sub-sections under the parent group's title.

- [ ] **Step 6: Compile and test**

Run: `cargo test -p anodizer-stage-changelog`

- [ ] **Step 7: Commit**

---

### Task 7: Sign Stage — Config + Wiring

**Files:**
- Modify: `crates/core/src/config.rs` — `SignConfig`, `DockerSignConfig`, add `BinarySignConfig`
- Modify: `crates/stage-sign/src/lib.rs` — wire fields

**Parity items addressed:**
- `signs[].output` (Missing → Implemented)
- `signs[].if` conditional (Missing → Implemented)
- `binary_signs[]` (Missing → Implemented)
- Docker `${digest}` template var (Missing → Implemented)
- Docker `${artifactID}` template var (Missing → Implemented)

- [ ] **Step 1: Add config fields**

To `SignConfig`:
```rust
pub struct SignConfig {
    // ... existing fields ...
    /// Capture stdout/stderr of the signing command.
    pub output: Option<bool>,
    /// Template-conditional: skip this sign config if rendered result is "true".
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
}
```

Add to top-level `Config`:
```rust
pub struct Config {
    // ... existing fields ...
    /// Binary-specific signing (signs individual binaries, not archives).
    pub binary_signs: Option<Vec<SignConfig>>,
}
```

- [ ] **Step 2: Wire output capture**

When `output == Some(true)`, capture the signing command's stdout and log it (or store it in artifact metadata).

- [ ] **Step 3: Wire if conditional**

Before processing a sign config, render the `if_condition` template. If result is `"false"` or empty, skip.

- [ ] **Step 4: Implement binary_signs**

Process `binary_signs` the same as `signs`, but filter to `ArtifactKind::Binary` only.

- [ ] **Step 5: Wire docker digest/artifactID template vars**

When signing docker images, set `digest` template var from the image digest metadata, and `artifactID` from the artifact's metadata `id` field.

- [ ] **Step 6: Compile and test**

Run: `cargo test -p anodizer-stage-sign`

- [ ] **Step 7: Commit**

---

### Task 8: Docker Stage — Config + Wiring

**Files:**
- Modify: `crates/core/src/config.rs` — `DockerConfig`, add `DockerManifestConfig`
- Modify: `crates/stage-docker/src/lib.rs` — wire fields

**Parity items addressed:**
- `skip_push` auto prerelease (Partial → Implemented)
- `docker[].use` (Missing → Implemented)
- Docker manifests (Missing → Implemented)
- Docker digest file (Missing → Implemented)

- [ ] **Step 1: Add config fields**

Change `DockerConfig.skip_push` to auto-or-bool:
```rust
pub struct DockerConfig {
    // ... existing fields ...
    /// Skip push: true, false, or "auto" (skip for prereleases).
    #[schemars(schema_with = "skip_push_schema")]
    pub skip_push: Option<SkipPushConfig>,
    /// Docker backend: "docker", "buildx" (default), or "podman".
    #[serde(rename = "use")]
    pub use_backend: Option<String>,
}
```

Add `SkipPushConfig`:
```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipPushConfig {
    Auto,
    Bool(bool),
}
impl_auto_or_bool_serde!(SkipPushConfig, SkipPushConfig::Auto, SkipPushConfig::Bool);
```

Add to `CrateConfig`:
```rust
pub struct CrateConfig {
    // ... existing fields ...
    pub docker_manifests: Option<Vec<DockerManifestConfig>>,
}
```

Add manifest config:
```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct DockerManifestConfig {
    pub name_template: String,
    pub image_templates: Vec<String>,
    pub create_flags: Option<Vec<String>>,
    pub push_flags: Option<Vec<String>>,
}
```

- [ ] **Step 2: Wire auto skip_push**

When `SkipPushConfig::Auto`, check `ctx.template_vars().get("Prerelease")` — skip push if prerelease is non-empty.

- [ ] **Step 3: Wire use_backend**

In `build_docker_command()`, switch between `docker build`, `docker buildx build`, and `podman build` based on `use_backend`.

- [ ] **Step 4: Implement docker manifests**

After building all docker images, create multi-arch manifests using `docker manifest create` + `docker manifest push`.

- [ ] **Step 5: Implement digest file**

After each docker build+push, run `docker inspect --format='{{.Id}}'` to get the digest, save to `dist/{image_name}.digest`.

- [ ] **Step 6: Compile and test**

Run: `cargo test -p anodizer-stage-docker`

- [ ] **Step 7: Commit**

---

### Task 9: nFPM Stage — Config + Wiring

**Files:**
- Modify: `crates/core/src/config.rs` — `NfpmConfig`, `NfpmFileInfo`
- Modify: `crates/stage-nfpm/src/lib.rs` — wire fields into YAML generation

**Parity items addressed:**
- `file_info.mtime` (Missing → Implemented)
- RPM-specific fields (Missing → Implemented)
- Deb-specific fields (Missing → Implemented)
- APK-specific fields (Missing → Implemented)
- Archlinux-specific fields (Missing → Implemented)
- `epoch`/`release`/`prerelease`/`version_metadata` (Missing → Implemented)
- `section`/`priority`/`meta`/`umask`/`mtime` (Missing → Implemented)
- Per-format signatures (Missing → Implemented)
- Termux deb / ipk formats (Missing → Implemented)

- [ ] **Step 1: Add config fields to NfpmConfig**

```rust
pub struct NfpmConfig {
    // ... existing fields ...
    /// Package epoch (for RPM/Deb versioning).
    pub epoch: Option<String>,
    /// Package release number.
    pub release: Option<String>,
    /// Prerelease suffix.
    pub prerelease: Option<String>,
    /// Additional version metadata.
    pub version_metadata: Option<String>,
    /// Package section (Deb).
    pub section: Option<String>,
    /// Package priority (Deb).
    pub priority: Option<String>,
    /// When true, this is a meta-package (no files, dependencies only).
    pub meta: Option<bool>,
    /// File permission umask.
    pub umask: Option<String>,
    /// Global modification time for reproducible builds.
    pub mtime: Option<String>,
    /// RPM-specific configuration.
    pub rpm: Option<NfpmRpmConfig>,
    /// Deb-specific configuration.
    pub deb: Option<NfpmDebConfig>,
    /// APK-specific configuration.
    pub apk: Option<NfpmApkConfig>,
    /// Archlinux-specific configuration.
    pub archlinux: Option<NfpmArchlinuxConfig>,
}
```

Add `mtime` to `NfpmFileInfo`:
```rust
pub struct NfpmFileInfo {
    pub owner: Option<String>,
    pub group: Option<String>,
    pub mode: Option<String>,
    pub mtime: Option<String>,
}
```

Add format-specific configs:
```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NfpmRpmConfig {
    pub summary: Option<String>,
    pub compression: Option<String>,
    pub group: Option<String>,
    pub packager: Option<String>,
    pub signature: Option<NfpmSignatureConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NfpmDebConfig {
    pub triggers: Option<NfpmDebTriggers>,
    pub breaks: Option<Vec<String>>,
    pub lintian_overrides: Option<Vec<String>>,
    pub signature: Option<NfpmSignatureConfig>,
    /// Additional Deb fields.
    pub fields: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NfpmDebTriggers {
    pub interest: Option<Vec<String>>,
    pub interest_await: Option<Vec<String>>,
    pub interest_noawait: Option<Vec<String>>,
    pub activate: Option<Vec<String>>,
    pub activate_await: Option<Vec<String>>,
    pub activate_noawait: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NfpmApkConfig {
    pub signature: Option<NfpmSignatureConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NfpmArchlinuxConfig {
    pub pkgbase: Option<String>,
    pub packager: Option<String>,
    pub scripts: Option<NfpmArchlinuxScripts>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NfpmArchlinuxScripts {
    pub preupgrade: Option<String>,
    pub postupgrade: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NfpmSignatureConfig {
    pub key_file: Option<String>,
    pub key_id: Option<String>,
    pub key_passphrase: Option<String>,
}
```

- [ ] **Step 2: Wire all fields to nFPM YAML generation**

In the nFPM stage's YAML builder, emit all new fields into the generated nfpm.yaml:
- `epoch`, `release`, `prerelease`, `version_metadata` as top-level version fields
- `section`, `priority`, `meta`, `umask`, `mtime` as package metadata
- `file_info.mtime` in contents entries
- Format-specific sections (`rpm:`, `deb:`, `apk:`, `archlinux:`)
- Per-format signature configurations

- [ ] **Step 3: Add ipk to formats list**

When format is `"ipk"` or `"termux-deb"`, pass through to nfpm (which supports these if the nfpm binary does).

- [ ] **Step 4: Compile and test**

Run: `cargo test -p anodizer-stage-nfpm`

- [ ] **Step 5: Commit**

---

### Task 10: Homebrew Publish — Config + Wiring

**Files:**
- Modify: `crates/core/src/config.rs` — `HomebrewConfig`, `TapConfig`
- Modify: `crates/stage-publish/src/homebrew.rs` — wire fields into formula

**Parity items addressed:**
- `commit_author.signing` (Missing → Implemented)
- `repository.branch` (Missing → Implemented)
- `repository.pull_request` (Missing → Implemented)
- `repository.git` SSH push (Missing → Implemented)
- `repository.token` config field (Partial → Implemented)
- `ids[]` archive filter (Missing → Implemented)
- `url_template` (Missing → Implemented)
- `url_headers` (Missing → Implemented)
- `download_strategy` / `custom_require` (Missing → Implemented)
- `custom_block` / `extra_install` / `post_install` (Missing → Implemented)
- `plist` / `service` (Missing → Implemented)
- Homebrew Casks (Missing → Implemented)

- [ ] **Step 1: Extend config structs**

Extend `TapConfig`:
```rust
pub struct TapConfig {
    pub owner: String,
    pub name: String,
    /// Branch to push to (default: main).
    pub branch: Option<String>,
    /// Token for authenticating to the tap repo.
    pub token: Option<String>,
    /// Create a pull request instead of direct push.
    pub pull_request: Option<TapPullRequestConfig>,
    /// Use SSH git URL for push instead of HTTPS.
    pub git: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct TapPullRequestConfig {
    pub enabled: Option<bool>,
    pub base: Option<String>,
    pub draft: Option<bool>,
}
```

Extend `HomebrewConfig`:
```rust
pub struct HomebrewConfig {
    // ... existing fields ...
    /// GPG-sign commits to tap.
    pub commit_author_signing: Option<bool>,
    /// Filter: only include archives whose id is in this list.
    pub ids: Option<Vec<String>>,
    /// Override the download URL template.
    pub url_template: Option<String>,
    /// HTTP headers for download URL.
    pub url_headers: Option<Vec<String>>,
    /// Custom download strategy class name.
    pub download_strategy: Option<String>,
    /// Ruby require statement for custom download strategy.
    pub custom_require: Option<String>,
    /// Arbitrary Ruby block inserted into the formula.
    pub custom_block: Option<String>,
    /// Extra install commands (Ruby code in install block).
    pub extra_install: Option<String>,
    /// Post-install Ruby code.
    pub post_install: Option<String>,
    /// Plist (launchd) service definition.
    pub plist: Option<String>,
    /// Homebrew service definition.
    pub service: Option<String>,
}
```

- [ ] **Step 2: Wire ids filter**

In `publish_homebrew()`, filter archive artifacts by `ids` before selecting the download URL.

- [ ] **Step 3: Wire url_template and url_headers**

When `url_template` is set, use it (rendered through template engine) as the download URL instead of deriving from GitHub release URL.

- [ ] **Step 4: Wire formula extras**

In formula generation, emit `download_strategy`, `custom_require`, `custom_block`, `extra_install`, `post_install`, `plist`, `service` into the Ruby formula template.

- [ ] **Step 5: Wire repository options**

Support `branch`, `pull_request`, `git` (SSH), and `token` in the tap push flow. When `pull_request.enabled`, use GitHub API to create a PR instead of direct push.

- [ ] **Step 6: Implement Homebrew Casks stub**

Add `HomebrewCaskConfig` to config.rs and a `publish_homebrew_cask()` function that generates a `.rb` cask file instead of a formula.

- [ ] **Step 7: Compile and test**

Run: `cargo test -p anodizer-stage-publish`

- [ ] **Step 8: Commit**

---

### Task 11: Scoop Publish — Config + Wiring

**Files:**
- Modify: `crates/core/src/config.rs` — `ScoopConfig`, `BucketConfig`
- Modify: `crates/stage-publish/src/scoop.rs` — wire fields

**Parity items addressed:**
- `commit_author.signing` (Missing → Implemented)
- `repository.branch` (Missing → Implemented)
- `repository.pull_request` (Missing → Implemented)
- `repository.git` SSH (Missing → Implemented)
- `repository.token` (Partial → Implemented)
- 32-bit architecture block (Missing → Implemented)
- `goamd64` (Missing → N/A for Rust)

- [ ] **Step 1: Extend BucketConfig**

```rust
pub struct BucketConfig {
    pub owner: String,
    pub name: String,
    pub branch: Option<String>,
    pub token: Option<String>,
    pub pull_request: Option<TapPullRequestConfig>,
    pub git: Option<String>,
}
```

Add to `ScoopConfig`:
```rust
pub struct ScoopConfig {
    // ... existing fields ...
    pub commit_author_signing: Option<bool>,
}
```

- [ ] **Step 2: Wire 32-bit architecture block**

In manifest generation, when archives for i686/x86 (32-bit) targets exist, add a `"32bit"` architecture block alongside the `"64bit"` block.

- [ ] **Step 3: Wire repository options**

Mirror the homebrew approach: support `branch`, `token`, `pull_request`, `git` (SSH) in bucket push flow.

- [ ] **Step 4: Compile and test**

Run: `cargo test -p anodizer-stage-publish`

- [ ] **Step 5: Commit**

---

### Task 12: Custom Publishers — Config + Wiring

**Files:**
- Modify: `crates/core/src/config.rs` — `PublisherConfig`
- Modify: `crates/cli/src/commands/publisher.rs` — wire fields

**Parity items addressed:**
- `meta` (Missing → Implemented)
- `extra_files` (Missing → Implemented)
- `output` capture stdout (Missing → Implemented)
- `if` per-artifact filter (Missing → Implemented)
- `templated_extra_files` (Missing → Implemented)
- Parallel per-artifact execution (Missing → Implemented)

- [ ] **Step 1: Add config fields**

```rust
pub struct PublisherConfig {
    // ... existing fields ...
    /// Include metadata artifacts.
    pub meta: Option<bool>,
    /// Additional files to include (glob patterns).
    pub extra_files: Option<Vec<String>>,
    /// Capture and log stdout/stderr from the command.
    pub output: Option<bool>,
    /// Per-artifact template condition: skip if rendered to "false" or empty.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
    /// Extra files with template rendering on paths.
    pub templated_extra_files: Option<Vec<String>>,
}
```

- [ ] **Step 2: Wire meta**

When `meta == Some(true)`, include `ArtifactKind::Metadata` artifacts in the filter.

- [ ] **Step 3: Wire extra_files**

Resolve glob patterns from `extra_files` and `templated_extra_files` (rendered through template engine), add them to the artifact list.

- [ ] **Step 4: Wire output capture**

When `output == Some(true)`, capture stdout/stderr and log it.

- [ ] **Step 5: Wire if condition**

For each artifact, render `if_condition` with artifact-scoped vars. Skip if result is `"false"` or empty.

- [ ] **Step 6: Wire parallel execution**

Use `rayon` or `std::thread` to run publisher commands for each artifact in parallel instead of sequentially.

- [ ] **Step 7: Compile and test**

Run: `cargo test -p anodizer-cli`

- [ ] **Step 8: Commit**

---

### Task 13: Announce Stage — Config + Wiring

**Files:**
- Modify: `crates/core/src/config.rs` — `AnnounceConfig`, provider structs
- Modify: `crates/stage-announce/src/lib.rs` — skip field
- Modify: `crates/stage-announce/src/teams.rs` — icon_url
- Modify: `crates/stage-announce/src/mattermost.rs` — title_template
- Modify: `crates/stage-announce/src/webhook.rs` — expected_status_codes
- Create: `crates/stage-announce/src/reddit.rs`, `mastodon.rs`, `bluesky.rs`, `linkedin.rs`, `discourse.rs`, `opencollective.rs`, `twitter.rs`
- Modify: `crates/stage-announce/src/lib.rs` — email SMTP transport

**Parity items addressed:**
- `announce.skip` (Missing → Implemented)
- Teams `icon_url` (Missing → Implemented)
- Mattermost `title_template` (Missing → Implemented)
- Webhook `expected_status_codes` (Missing → Implemented)
- Email SMTP transport (Partial → Implemented)
- Reddit/Twitter/Mastodon/Bluesky/LinkedIn/OpenCollective/Discourse (Missing → Implemented)

- [ ] **Step 1: Add announce.skip and provider fields to config**

```rust
pub struct AnnounceConfig {
    /// Template-conditional skip for all announcements.
    pub skip: Option<String>,
    // ... existing fields ...
    pub reddit: Option<RedditAnnounce>,
    pub twitter: Option<TwitterAnnounce>,
    pub mastodon: Option<MastodonAnnounce>,
    pub bluesky: Option<BlueskyAnnounce>,
    pub linkedin: Option<LinkedInAnnounce>,
    pub opencollective: Option<OpenCollectiveAnnounce>,
    pub discourse: Option<DiscourseAnnounce>,
}
```

Add to `TeamsAnnounce`:
```rust
pub struct TeamsAnnounce {
    // ... existing fields ...
    pub icon_url: Option<String>,
}
```

Add to `MattermostAnnounce`:
```rust
pub struct MattermostAnnounce {
    // ... existing fields ...
    pub title_template: Option<String>,
}
```

Add to `WebhookConfig`:
```rust
pub struct WebhookConfig {
    // ... existing fields ...
    pub expected_status_codes: Option<Vec<u16>>,
}
```

Add to `EmailAnnounce`:
```rust
pub struct EmailAnnounce {
    // ... existing fields ...
    pub smtp_host: Option<String>,
    pub smtp_port: Option<u16>,
    pub smtp_username: Option<String>,
    pub smtp_password: Option<String>,
}
```

Add provider configs:
```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct RedditAnnounce {
    pub enabled: Option<bool>,
    pub subreddit: Option<String>,
    pub title_template: Option<String>,
    pub message_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct MastodonAnnounce {
    pub enabled: Option<bool>,
    pub server: Option<String>,
    pub message_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct BlueskyAnnounce {
    pub enabled: Option<bool>,
    pub message_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct TwitterAnnounce {
    pub enabled: Option<bool>,
    pub message_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct LinkedInAnnounce {
    pub enabled: Option<bool>,
    pub message_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct OpenCollectiveAnnounce {
    pub enabled: Option<bool>,
    pub title_template: Option<String>,
    pub message_template: Option<String>,
    pub slug: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct DiscourseAnnounce {
    pub enabled: Option<bool>,
    pub url: Option<String>,
    pub category_id: Option<u64>,
    pub title_template: Option<String>,
    pub message_template: Option<String>,
}
```

- [ ] **Step 2: Wire announce.skip**

In the announce stage entry point, render `skip` template and abort if result is `"true"`.

- [ ] **Step 3: Wire Teams icon_url**

In teams.rs, include `icon_url` in the Adaptive Card payload.

- [ ] **Step 4: Wire Mattermost title_template**

In mattermost.rs, render `title_template` and include it as the attachment title.

- [ ] **Step 5: Wire Webhook expected_status_codes**

In webhook.rs, after sending the request, check response status against `expected_status_codes` (default: `[200, 201, 202, 204]`). Error if not in list.

- [ ] **Step 6: Implement SMTP email transport**

When `smtp_host` is configured, use `lettre` crate (add to deps) to send via SMTP instead of sendmail.

- [ ] **Step 7: Implement social media providers**

For each new provider (Reddit, Twitter/X, Mastodon, Bluesky, LinkedIn, OpenCollective, Discourse), create a module file with:
1. Config struct reading
2. API authentication (via env vars for tokens)
3. Message template rendering
4. HTTP POST to provider API
5. Dry-run logging

- [ ] **Step 8: Compile and test**

Run: `cargo test -p anodizer-stage-announce`

- [ ] **Step 9: Commit**

---

### Task 14: CLI Flags + Commands

**Files:**
- Modify: `crates/cli/src/lib.rs` — add flags and man command
- Modify: `crates/core/src/context.rs` — add fail_fast to ContextOptions
- Modify: `crates/cli/src/pipeline.rs` — wire fail_fast
- Add: `clap_mangen` dependency

**Parity items addressed:**
- `--fail-fast` (Missing → Implemented)
- `--release-notes-tmpl` (Missing → Implemented)
- `--output` / `-o` for build (Missing → Implemented)
- `man` command (Missing → Implemented)

- [ ] **Step 1: Add --fail-fast flag**

In the `Release` command in `lib.rs`:
```rust
#[arg(long, help = "Stop immediately on first stage error")]
fail_fast: bool,
```

Add to `ContextOptions`:
```rust
pub fail_fast: bool,
```

- [ ] **Step 2: Add --release-notes-tmpl flag**

```rust
#[arg(long, help = "Go template string for release notes (rendered with template vars)")]
release_notes_tmpl: Option<String>,
```

When set, render the template and use as release notes.

- [ ] **Step 3: Add --output/-o to Build command**

```rust
Build {
    // ... existing fields ...
    #[arg(long, short = 'o', help = "Output directory for built binaries")]
    output: Option<PathBuf>,
}
```

- [ ] **Step 4: Add Man command**

Add `clap_mangen = "0.2"` to workspace deps. Add command:
```rust
/// Generate man page
Man {
    #[arg(long, help = "Output directory for man pages")]
    dir: Option<PathBuf>,
},
```

In the command handler, use `clap_mangen::Man::new()` to generate man pages.

- [ ] **Step 5: Wire fail_fast in pipeline**

In `Pipeline::run()`, when `fail_fast` is set and a stage fails, return immediately instead of continuing.

- [ ] **Step 6: Compile and test**

Run: `cargo test -p anodizer-cli`

- [ ] **Step 7: Commit**

---

### Task 15: Global Hooks — `if` Conditional

**Files:**
- Modify: `crates/core/src/config.rs` — `StructuredHook`
- Modify: `crates/cli/src/pipeline.rs` — wire if conditional

**Parity items addressed:**
- `if` conditional on hooks (Missing → Implemented)

- [ ] **Step 1: Add if field to StructuredHook**

```rust
pub struct StructuredHook {
    pub cmd: String,
    pub dir: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub output: Option<bool>,
    /// Template-conditional: skip this hook if rendered result is "false" or empty.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
}
```

- [ ] **Step 2: Wire in run_hooks()**

In `pipeline.rs:run_hooks()`, when processing a `Structured` hook with `if_condition`, render the template. Skip if result is `"false"`, empty, or whitespace-only.

- [ ] **Step 3: Compile and test**

Run: `cargo test -p anodizer-cli`

- [ ] **Step 4: Commit**

---

### Task 16: Other Publishers — Nix + Snapcraft

**Files:**
- Modify: `crates/core/src/config.rs` — add `NixConfig`, `SnapcraftPublishConfig`
- Modify: `crates/stage-publish/src/lib.rs` — integrate new publishers
- Create: `crates/stage-publish/src/nix.rs`
- Modify: `crates/stage-snapcraft/src/lib.rs` — add publish support

**Parity items addressed:**
- Nix publisher (Missing → Implemented)
- Snapcraft publish (Missing → Implemented)

- [ ] **Step 1: Add NixConfig**

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NixConfig {
    pub repository: Option<NixRepoConfig>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub path: Option<String>,
    pub commit_msg_template: Option<String>,
    pub commit_author_name: Option<String>,
    pub commit_author_email: Option<String>,
    pub skip_upload: Option<String>,
    pub url_template: Option<String>,
    pub install: Option<String>,
    pub extra_install: Option<String>,
    pub post_install: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NixRepoConfig {
    pub owner: String,
    pub name: String,
    pub branch: Option<String>,
    pub token: Option<String>,
}
```

Add to `PublishConfig`:
```rust
pub struct PublishConfig {
    // ... existing fields ...
    pub nix: Option<NixConfig>,
}
```

- [ ] **Step 2: Implement nix.rs**

Generate a Nix derivation file (`default.nix`) with:
- `pname`, `version`, `src` (fetchurl with sha256)
- `installPhase` from config
- Platform-specific source selection

Push to the configured repository (same flow as homebrew/scoop).

- [ ] **Step 3: Wire snapcraft publish**

In the snapcraft stage, when `publish: true` is set, run `snapcraft upload` + `snapcraft release` after building the snap.

- [ ] **Step 4: Compile and test**

Run: `cargo test -p anodizer-stage-publish`

- [ ] **Step 5: Commit**

---

### Task 17: Go-Specific Items — Mark N/A

**Files:**
- Modify: `.claude/specs/goreleaser-parity-matrix.md`

**Parity items addressed:**
- `gomod proxy` → N/A (Go-specific)
- `no_main_check` → N/A (Go-specific, Rust uses crate types)
- `goamd64` microarch → N/A (Go-specific)
- `ModulePath` template var → N/A (Go-specific)
- Multiple builders (go/zig/deno/bun) → N/A (Rust-focused tool; cross-compilation handles multi-target)

- [ ] **Step 1: Update parity matrix**

Change status of Go-specific items to `Implemented (N/A)` with notes explaining these are Go-specific features with Rust equivalents already present.

---

### Task 18: Update Parity Matrix — Mark All Closures

**Files:**
- Modify: `.claude/specs/goreleaser-parity-matrix.md`

- [ ] **Step 1: Update every row**

After all tasks complete, change every Missing/Partial to Implemented with notes on what was done.

- [ ] **Step 2: Final audit scan**

Re-read each table row and verify the implementation exists in code.

---

## Execution Notes

**Dependency order:** Task 1 (done) → Tasks 2-16 can run in parallel (each touches different stage files) → Task 17-18 (final).

**Shared file conflict:** All tasks modify `config.rs`. When using parallel agents, each task's config changes must be applied sequentially or coordinated. Recommend: one agent handles all config changes, then dispatches stage agents.

**Test strategy:** Each task runs `cargo test -p <crate>` after implementation. Final validation: `cargo test --workspace`.
