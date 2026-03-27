# Community Adoption Feature Gap Analysis

> **Surveyed:** 2026-03-27 — 21 popular Rust CLI projects, analyzing their actual GitHub Actions release workflows.
>
> **Purpose:** Identify features anodize must add before submitting community adoption PRs, and unique features that could differentiate anodize.
>
> **Usage:** The community adoption session should review this spec, do a fresh search to verify currency, determine which features are in scope, and finalize a prioritized implementation list before starting any PR work.

---

## Methodology

Fetched and analyzed the actual `.github/workflows/release.yml` (and related files) from 21 projects totaling ~400k GitHub stars. Every release-relevant step was cataloged and cross-referenced against anodize's current feature set.

### Projects Analyzed

| Project | Stars | Workflow Files Analyzed |
|---------|-------|----------------------|
| BurntSushi/ripgrep | 61.5k | release.yml (373 lines) |
| sharkdp/bat | 57.8k | CICD.yml (464 lines) |
| starship/starship | 55.4k | release.yml (408 lines) |
| nushell/nushell | 38.9k | release.yml, release-msi.yml, nightly-build.yml, winget-submission.yml |
| sharkdp/fd | 42.2k | CICD.yml (308 lines) |
| sharkdp/hyperfine | 27.8k | CICD.yml (357 lines) |
| ajeetdsouza/zoxide | 35.0k | release.yml |
| dandavison/delta | 29.7k | cd.yml (108 lines) |
| helix-editor/helix | 43.7k | release.yml (301 lines) |
| extrawurst/gitui | 21.6k | cd.yml, nightly.yml, brew.yml |
| eza-community/eza | 20.8k | apt.yml, winget.yml |
| sxyazi/yazi | 35.4k | publish.yml |
| casey/just | 32.4k | release.yaml |
| orhun/git-cliff | 11.6k | cd.yml |
| Wilfred/difftastic | 24.8k | release.yml |
| biomejs/biome | 24.2k | release_cli.yml |
| orf/gping | 12.4k | homebrew.yml, winget.yml, docker.yml |
| astral-sh/ruff | 46.7k | (uses cargo-dist — excluded from feature analysis) |
| prefix-dev/pixi | 6.7k | (uses cargo-dist — excluded from feature analysis) |
| XAMPPRocky/tokei | 14.1k | mean_bean_ci.yml |
| Orange-OpenSource/hurl | 18.7k | release.yml |

---

## A. Features Shared Across Many Projects — anodize Doesn't Offer

These are features that 3+ projects implement in their release workflows. Without them, those projects cannot switch to anodize.

### 1. Shell Completion Generation

**Used by:** ripgrep, bat, fd, hyperfine, zoxide, git-cliff, just (7 projects)

Every serious CLI tool generates shell completions (bash, fish, zsh, PowerShell) at build time and bundles them in release archives and deb/rpm packages. Most use clap's built-in `clap_complete` or a `--generate` flag. The completions are placed in a known directory structure and included in archives alongside the binary.

**anodize gap:** No concept of "generate files at build time and include in archives." The build stage compiles binaries; the archive stage packages them. There's no hook between them for generated artifacts.

**Possible implementation:** Add `generate` hooks to build config — commands that run after compilation and produce files that are automatically included in archives. Or a dedicated `completions` config field that generates them via a configurable command.

### 2. Man Page Generation

**Used by:** ripgrep, bat, fd, hyperfine, zoxide, just, git-cliff (7 projects)

Same pattern as completions — man pages are generated from `--help` output, a dedicated `--man` flag, or pre-built `.1` files. They're bundled in archives and installed by deb/rpm packages.

**anodize gap:** Same as completions — no post-build artifact generation.

**Possible implementation:** Same mechanism as completions. A `generate` block or `manpages` config field.

### 3. Binary Stripping

**Used by:** ripgrep, bat, delta, biome, fd (5 projects)

Most projects strip debug symbols from release binaries to reduce size (often 50-80% reduction). This is done via `strip` command, `RUSTFLAGS='-C strip=symbols'`, or architecture-specific strip tools for cross-compiled targets.

**anodize gap:** No stripping support. Binaries are shipped with debug symbols.

**Possible implementation:** Add `strip: true` to build config (default false). When enabled, either pass `-C strip=symbols` to rustc via RUSTFLAGS, or run the platform-appropriate `strip` command post-build. Per-target strip configuration may be needed for cross-compilation.

### 4. Prerelease Detection from Tag Format

**Used by:** gitui, git-cliff, just, starship (4 projects)

Tags containing `-` (e.g., `v1.0.0-rc1`, `v2.0.0-beta.3`) automatically mark the GitHub release as a prerelease. This is distinct from the explicit `prerelease: true` config — it's automatic based on semver prerelease semantics.

**anodize gap:** Has `prerelease: auto` but this currently maps to a config value, not tag-format detection.

**Possible implementation:** When `prerelease: auto`, parse the tag for semver prerelease segments (anything after `-`). If present, mark as prerelease automatically.

### 5. Changelog Extraction from Existing CHANGELOG.md

**Used by:** gitui, git-cliff, bat (3+ projects)

Several projects maintain a hand-written or tool-generated `CHANGELOG.md` and extract the section for the current version as release notes, rather than generating notes from git history.

**anodize gap:** Changelog stage generates from git commits only. No option to extract from an existing file.

**Possible implementation:** Add `changelog.use: file` mode with `changelog.file: CHANGELOG.md` — extracts the section matching the current version/tag.

### 6. Build Provenance / SLSA Attestation

**Used by:** fd, biome (2 projects, but growing fast)

GitHub's `actions/attest-build-provenance` generates signed SLSA provenance statements for artifacts, enabling supply-chain verification. This is becoming a requirement for security-conscious organizations.

**anodize gap:** No attestation support.

**Possible implementation:** Add `attestation: true` to build or release config. In the GitHub Action, call the attestation action after build. For CLI-only usage, generate an in-toto statement.

### 7. Multi-arch Docker via BuildX

**Used by:** just, gping (2 projects explicitly, but standard practice)

Docker multi-platform builds using `docker buildx build --platform linux/amd64,linux/arm64,linux/arm/v7` with QEMU emulation. Produces a multi-arch manifest so users get the right image automatically.

**anodize gap:** Docker stage builds single-platform images. No BuildX multi-platform support.

**Possible implementation:** Add `platforms: [linux/amd64, linux/arm64]` to docker config. When set, use BuildX with QEMU for multi-platform manifest builds.

---

## B. Unique Features Per Project — Development Candidates

Each of these is a feature seen in one specific project that anodize doesn't offer but could develop, potentially benefiting multiple adopters.

### From ripgrep: Cross-Architecture Test Execution via QEMU

Tests run on non-native architectures during release builds using QEMU emulation through the `cross` tool. Catches architecture-specific bugs before release.

**Candidate feature:** `test_on_target: true` in build config — runs `cargo test` via cross/QEMU for each non-native target.

### From bat: Deb Package Variant Conflicts

Generates separate musl and glibc deb packages with `Conflicts:` declarations so the package manager prevents installing both. Also generates proper `Provides:` metadata.

**Candidate feature:** Enhance nfpm config to support `conflicts`, `provides`, and variant-aware packaging.

**Note:** anodize's nfpm stage already has `conflicts` in its config schema (added in Session 2F). Verify this is actually wired through.

### From starship: macOS Code Signing + Notarization

Full Apple Developer ID signing flow: keychain creation, certificate import, `codesign` execution, then `notarytool submit` for Apple notarization. Also builds `.pkg` installers with `pkgbuild`/`productbuild` and notarizes those.

**Candidate feature:** A `codesign` stage or extension to the sign stage for macOS-specific signing + notarization. Requires Apple Developer ID certificate and team credentials.

**Note:** This is critical for macOS distribution. Unsigned/unnotarized binaries trigger Gatekeeper warnings.

### From nushell: Nightly Old-Release Cleanup

Automatically deletes nightly GitHub releases older than N, keeping only the most recent 10. Prevents nightly tag/release accumulation.

**Candidate feature:** `nightly.keep_last: 10` config — after creating a new nightly release, prune older ones via GitHub API.

### From helix: AppImage Generation

Creates Linux AppImage bundles using `linuxdeploy` with zsync metadata for delta updates. AppImage is Linux's "universal binary" — runs on any distro without installation.

**Candidate feature:** Add AppImage as an archive format or a new packaging stage. Requires `linuxdeploy` tool and `.desktop` file configuration.

**Note:** Already planned in Session 9 scope but not yet detailed.

### From just: Documentation Site Deployment (mdbook + GitHub Pages)

Builds documentation with mdbook and deploys to GitHub Pages as part of the release workflow. Ensures docs are always in sync with releases.

**Candidate feature:** anodize already has mdBook in Session 5O. Could add a `deploy.github_pages` config to automate deployment during release.

### From git-cliff: NPM + PyPI Publishing

Wraps native Rust binaries as npm packages (platform-specific) and Python wheels (via maturin). Makes Rust CLI tools installable via `npm install -g <tool>` and `pip install <tool>`.

**Candidate feature:** Add `publish.npm` and `publish.pypi` config sections. NPM requires generating per-platform packages with optionalDependencies. PyPI requires maturin for wheel building.

**Impact:** Opens anodize to the massive npm/PyPI user bases. git-cliff and biome both do this.

### From biome: WASM Compilation

Compiles to `wasm32-unknown-unknown` via `wasm-pack` for bundler, nodejs, and web targets. Enables browser and Node.js usage of Rust tools.

**Candidate feature:** Recognize `wasm32-*` targets in build config, use `wasm-pack` instead of `cargo build`. Register `.wasm` as artifact type.

### From gping: Docker Build Cache via GitHub Actions Cache

Uses `--cache-from type=gha --cache-to type=gha,mode=max` with BuildX for layer caching across CI runs. Dramatically speeds up Docker builds.

**Candidate feature:** Add `cache: true` to docker config. When running in GitHub Actions, emit the appropriate cache flags.

### From zoxide: Android Targets

Builds for `aarch64-linux-android` and `armv7-linux-androideabi`. These are valid Rust targets for Android deployment.

**Candidate feature:** Recognize Android target triples in the build stage. May require NDK toolchain configuration.

### From gitui: AWS S3 Nightly Upload

Uploads nightly builds to an S3 bucket rather than GitHub releases. Useful for organizations that host their own artifact storage.

**Candidate feature:** Already planned in Session 10 (blob storage). Validates the need.

### From eza: Self-Hosted APT Repository

Maintains a self-hosted APT repository with signed packages at a custom domain, rather than relying on GitHub releases for deb distribution.

**Candidate feature:** Add `publish.apt` config for pushing deb packages to a hosted APT repository. Requires GPG signing and repository metadata generation.

### From fd: SLSA Build Attestation

Uses `actions/attest-build-provenance` for supply-chain security. Covered in Section A.6 above.

---

## C. Priority Recommendations

### Must-Have Before Community PRs

Without these, the target projects cannot realistically switch to anodize:

1. **Shell completion generation** — 7/21 projects need this
2. **Man page generation** — 7/21 projects need this
3. **Binary stripping** — 5/21 projects need this
4. **Prerelease auto-detection from tag** — 4/21 projects expect this

### High Value for Differentiation

These would make anodize's value proposition compelling beyond "replaces your YAML":

5. **macOS code signing + notarization** — starship's #1 pain point
6. **AppImage generation** — helix needs this, broad Linux value
7. **NPM/PyPI publishing** — opens anodize to non-Rust consumers (git-cliff, biome)
8. **Multi-arch Docker (BuildX)** — modern container standard
9. **Build provenance attestation** — growing security requirement

### Nice-to-Have

10. Nightly release cleanup
11. Changelog extraction from existing file
12. WASM compilation support
13. Android target support
14. Docker build cache flags
15. APT repository publishing

### Probably Out of Scope

- QEMU test execution (CI concern, not release tooling)
- MSRV testing (CI concern)
- Crowdin i18n integration (too niche)
- Documentation site deployment (tangential to release)

---

## D. Existing Tool Landscape

Only 2 of 21 surveyed projects use any release automation tool:

| Tool | Projects | Notes |
|------|----------|-------|
| **cargo-dist** | ruff, pixi | Handles binary building + distribution, but ruff still has 19 workflow files / 4,600 lines |
| **GoReleaser** | 0 | None of the Rust projects surveyed use it |
| **release-plz** | 0 | Handles versioning + crates.io only, not binary distribution |

The remaining 19 projects all roll their own GitHub Actions YAML. This is the opportunity.
