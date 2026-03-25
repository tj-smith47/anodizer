# Anodize — Global Roadmap

**Status:** Post-Release 1 implementation. All core features implemented (138 tests, 0 clippy warnings, ~8k LOC).

**This file is the backlog for future sessions.** Each section is an initiative. The next session should start with a fresh parity gap analysis (Initiative 1) and use its findings to update this plan.

### What MUST happen after release (depends on crates.io publish)

These initiatives cannot start until anodize is published and installable:

- **Initiative 2** (Full GitHub Action) — the action downloads and installs the published binary
- **Initiative 7** (cfgd migration) — cfgd's workflow needs to `cargo install anodize` or use the action
- **Initiative 6** (Community PRs) — can't submit PRs to other repos until the tool is installable

Everything else (gap analysis, docs site, test coverage, Rust-specific features, Release 2 features, auto-tagging) can be worked on before or in parallel with release.

---

## Initiative 1: Fresh GoReleaser Parity Gap Analysis

**Priority:** First thing in next session.

Do a comprehensive, fresh evaluation of anodize vs GoReleaser OSS. Don't rely on what's already documented — go read GoReleaser's current source, docs, and feature list. Produce a gap report covering:

1. **Feature-by-feature comparison** — every GoReleaser OSS feature, whether anodize has it, and what's missing
2. **CLI parity** — compare every goreleaser CLI flag/command vs anodize
3. **Config schema parity** — diff GoReleaser's `.goreleaser.yaml` schema against `.anodize.yaml`
4. **GitHub Action parity** — compare goreleaser-action features vs our composite action
5. **Test coverage comparison** — GoReleaser's test suite structure vs ours (unit, integration, e2e)
6. **Documentation parity** — goreleaser.com site structure and content coverage

Output: a gap analysis document at `.claude/specs/parity-gap-analysis.md` that feeds into the other initiatives.

---

## Initiative 2: Full-Featured GitHub Action

**Repo:** `tj-smith47/anodize-action` (separate repo)

Build a JavaScript/TypeScript GitHub Action matching goreleaser-action's feature set:
- Binary download + caching via `@actions/tool-cache`
- Structured outputs (artifacts, metadata) via `@actions/core`
- Grouped logging in Actions UI
- `install-only` mode
- Version pinning
- Cross-platform runner support

---

## Initiative 3: Documentation Site

Build a documentation site (likely using mdBook, Zola, or similar) comparable to goreleaser.com:
- Getting started guide
- Full configuration reference (already exists at `docs/configuration.md`)
- Per-stage documentation
- Migration guide from GoReleaser
- Migration guide from manual release workflows
- CI/CD integration guides (GitHub Actions, GitLab CI, etc.)
- FAQ
- Deploy to GitHub Pages or similar

---

## Initiative 4: Test Coverage Parity

Current: 138 tests (mostly unit + some integration).
GoReleaser: thousands of tests including extensive integration and e2e.

Gaps to address:
- End-to-end pipeline tests (`release --snapshot` in a real Cargo project)
- Per-stage integration tests with real artifacts (not just mocked contexts)
- Error path coverage (invalid configs, failed builds, missing tools, network errors)
- Cross-platform testing (Windows, macOS)
- Workspace-aware tests (multi-crate release flows)
- Regression tests for review-identified edge cases

---

## Initiative 5: Rust-Specific First-Class Features

Evaluate and implement features unique to the Rust ecosystem that don't have Go equivalents but are essential for a Rust release tool:

- **`cargo-binstall` metadata** — generate binstall-compatible metadata so users can `cargo binstall <tool>` from GitHub releases
- **`rust-toolchain.toml` awareness** — detect and respect MSRV, required components
- **MSRV checking** — verify binaries build against the declared minimum supported Rust version
- **Workspace dependency version sync** — detect and handle version mismatches between workspace crates
- **`cargo-dist` migration path** — provide a migration guide and compatibility layer for projects using cargo-dist
- **Conditional compilation features** — release builds with specific feature flag combinations
- **`cdylib` / `staticlib` support** — release shared/static libraries, not just binaries
- **`wasm32` target support** — first-class support for WebAssembly builds and packaging
- **Crate documentation builds** — optionally build and publish docs.rs-compatible documentation

The next session should evaluate which of these are "must-have" vs "nice-to-have" based on what popular Rust projects actually need (see Initiative 6).

---

## Initiative 6: Community Adoption — Popular Repo PRs

Identify popular Rust projects with release workflows that could benefit from anodize. Strategy:

1. **Survey popular Rust repos** — find projects using:
   - Manual GitHub Actions release workflows (like cfgd's current workflow)
   - `cargo-dist`
   - Custom shell scripts
   - GoReleaser (yes, some Rust projects use it)
2. **Identify common pain points** — what do their workflows struggle with?
   - Multi-crate workspace releases
   - Cross-compilation setup
   - Platform-specific packaging
   - Changelog generation
3. **Implement targeted features** — if common patterns emerge, add first-class support
   - "We added feature X specifically to solve problem Y that we found in N popular Rust repos"
4. **Submit PRs** — convert their workflows to `.anodize.yaml` + minimal GitHub Actions
   - Each PR demonstrates value and introduces anodize to a new community

Target repos to evaluate (examples):
- `ripgrep` — complex release with multiple platforms
- `bat` — similar to ripgrep
- `starship` — cross-platform with many targets
- `nushell` — workspace with multiple crates
- `zoxide` — simpler release, good starter PR
- `tokio` — workspace with independent crate releases
- `serde` — workspace with publish ordering
- `clap` — workspace with many crates

---

## Initiative 7: cfgd Migration — First Real-World Adoption

**Depends on:** Anodize Release 1 published to crates.io.

Convert cfgd's 633-line release workflow to an `.anodize.yaml` config. This serves as:
- The first real-world test of anodize on a production Rust project
- A showcase for the README ("See anodize in action on [cfgd](https://github.com/tj-smith47/cfgd)")
- A source of feature gaps we missed

**cfgd's release workflow does things anodize doesn't yet cover:**
- **Helm chart** packaging and OCI registry push (`helm package` + `helm push`)
- **Krew manifest** generation for kubectl plugin distribution
- **Crossplane function** xpkg build and push
- **OLM bundle** for Operator Lifecycle Manager
- **Cargo.toml version sync** from git tag — cfgd uses `sed` to update `version = "..."` in Cargo.toml before `cargo publish` because the workspace version doesn't track the git tag. This is a common Rust release pattern that anodize should handle natively (either a `version_from: tag` config option or a pre-publish hook that syncs automatically).
- **Multiple Dockerfiles** per crate — cfgd has `Dockerfile.operator.release`, `Dockerfile.agent.release`, `Dockerfile.csi.release`. Anodize supports this via multiple `docker` entries per crate.

**Tasks:**
1. Write `.anodize.yaml` for cfgd (exercises multi-crate, multi-docker, homebrew, krew, crates.io with ordering)
2. Identify which cfgd features need new anodize capabilities (Helm, Krew, Crossplane, OLM → likely `after` hooks or new stages)
3. Evaluate whether Cargo.toml version sync should be a first-class feature
4. Replace the 633-line workflow with `uses: tj-smith47/anodize@v1` + minimal config
5. Add cfgd as a showcase link in anodize's README
6. Submit PR to cfgd repo

---

## Initiative 8: Built-in Auto-Tagging

**Improvement over GoReleaser** — GoReleaser requires the tag to exist before running. cfgd uses a separate `AutoTag` GitHub Action (`anothrNick/github-tag-action@1.71.0`) to handle this. Anodize should offer auto-tagging as a built-in feature, modeled after that action's behavior. This eliminates the need for a separate workflow step and is something no other release tool does natively.

Semantics to be brainstormed in a dedicated session.

---

## Initiative 9: Release 2 Features (Already Planned)

From the spec — these are GoReleaser Pro features, free in anodize:

- **Monorepo support** — multiple independent workspaces in one repo
- **Nightly builds** — `--nightly` flag
- **Config includes/templates** — split config across files
- **Split/merge** — fan out builds across CI runners, merge artifacts
- **Snapcraft** — Linux snap packaging
- **dmg, msi, pkg** — native OS installers
- **Chocolatey, Winget** — Windows package managers
- **Reproducible builds** — deterministic output with `SOURCE_DATE_EPOCH`
- **macOS Universal Binaries** — fat binaries combining x86_64 + aarch64

Plus from the gap analysis (Initiative 1), there may be additional features to add here.

---

## Initiative 10: Ongoing Maintenance

- Address `serde_yaml` deprecation (migrate to `serde_yml` or alternative when stable)
- Keep `octocrab`, `reqwest`, `clap` dependencies updated
- CI pipeline for anodize itself (dogfood!)
- Publish to crates.io
- Community engagement (README badges, contribution guidelines, issue templates)
