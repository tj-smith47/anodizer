# Anodize — Unified Plan

> **For agentic workers:** Use superpowers:subagent-driven-development to implement each session's tasks in parallel where possible. Each session is designed for one conversation. Sessions are sequential — complete Session N before starting Session N+1. **Run a superpowers:code-reviewer agent between each task** to verify the work before moving on.

**Status:** Core implementation complete (138 tests, 0 clippy warnings, ~8k LOC). Gap analysis done. Ready for parity push.

**Reference docs:**
- Architecture & design: `.claude/specs/2026-03-25-anodize-design.md`
- Full gap analysis: `.claude/specs/parity-gap-analysis.md`

---

## Session 1: Tera Template Engine + P0 Gaps

**Why first:** The template engine is the foundation — every stage uses it. The current engine is a 70-line regex substitution that only does `{{ .Var }}` → value replacement. It has no conditionals, no functions, no pipes. This blocks real-world usage. P0 CLI/config gaps are small and should be done alongside.

**Prerequisite:** `cargo add tera` in `crates/core/`

### Task 1A: Migrate template engine to Tera (core)
- Replace regex engine in `crates/core/src/template.rs` with the `tera` crate
- Add a Go-style preprocessor: convert `{{ .Field }}` → `{{ Field }}` and `{{ .Env.VAR }}` → Tera-compatible access before passing to Tera
- Keep backward compat: both `{{ .Field }}` and `{{ Field }}` should work
- Tera gives us for free: `if`/`else`/`endif`, `for` loops, pipes (`| lower`, `| upper`, `| replace`), `| default`, `| trim`, `| title`, and many more built-in filters
- Update all existing tests to work with Tera

**Done when:** All existing template tests pass with the Tera backend. `{{ .ProjectName }}` and `{{ ProjectName }}` both resolve. `{{ .Env.VAR }}` works.

### Task 1B: Tera custom filters + new template tests
- Register GoReleaser-compat aliases: `tolower` → `lower`, `toupper` → `upper`
- Register custom filters: `trimprefix`, `trimsuffix`
- Add tests for: conditionals (`{% if IsSnapshot %}...{% endif %}`), pipes (`{{ Version | upper }}`), default values (`{{ Branch | default(value="main") }}`), nested access

**Done when:** Tests cover conditionals, pipes, functions, and error cases (undefined var, bad syntax). At least 8 new template tests.

### Task 1C: Add missing template variables
- Populate in `Context::new()` and during pipeline execution
- Global variables (from git): `Branch` (git rev-parse --abbrev-ref HEAD), `PreviousTag` (previous matching tag), `CommitDate` (git log -1 --format=%aI), `CommitTimestamp` (git log -1 --format=%at), `RawVersion` (version without `v` prefix), `IsGitDirty` (git status --porcelain), `GitTreeState` ("clean"/"dirty"), `Now` (UTC ISO 8601), `Commit` (alias for FullCommit)
- Stage-scoped variables (set per-artifact during stage execution): `Binary`, `ArtifactName`, `ArtifactPath`
- Audit: ensure `Date`, `Timestamp`, `FullCommit`, `IsSnapshot`, `IsDraft`, `Prerelease` are populated at runtime, not just in tests

**Done when:** Each new variable has a test. Stage-scoped vars are documented in code comments noting which stages set them.

### Task 1D: `--config` / `-f` flag
- Add `--config` / `-f` global flag to CLI (all commands: release, build, check, init, changelog)
- Update `pipeline::find_config()` to accept an optional path override
- Default behavior unchanged (search CWD for `.anodize.yaml` etc.)

**Done when:** `anodize check -f path/to/config.yaml` works. Test exercises non-default config path.

### Task 1E: `--timeout` flag
- Add `--timeout` flag to `release` and `build` commands (default: 30m)
- Wrap pipeline execution in a timeout using `std::thread` with a deadline (avoid adding tokio as a dep if not already present)

**Done when:** `--timeout 5s` on a long operation produces a timeout error. Test verifies timeout behavior.

### Task 1F: Config schema P0 additions
- Add `make_latest: true/false/auto` to `ReleaseConfig` (config field only — API wiring is Task 2C)
- Add `header` and `footer` (string) to `ChangelogConfig`; update changelog stage to prepend/append
- Add `disable: bool` to `ChecksumConfig` and `ChangelogConfig`; update stages to skip when disabled
- Update `check` command to validate new fields

**Done when:** Each new field has a config parsing test and a stage behavior test. Changelog with header/footer produces correct output. Disabled checksum/changelog stages are skipped.

### Task 1G: Auto-detect GitHub owner/name from git remote
- Parse `git remote get-url origin` to extract owner/name when `release.github` is omitted
- Handle HTTPS (`https://github.com/owner/repo.git`) and SSH (`git@github.com:owner/repo.git`) formats

**Done when:** `release` config without explicit `github.owner`/`github.name` infers them from git remote. Test covers both URL formats and error case (no remote).

### Task 1H: Update dogfood config and docs
- Update `.anodize.yaml` and `docs/configuration.md` to reflect new template syntax and fields
- Update design spec's Template Engine section to reflect Tera decision (currently says "intentionally minimal custom engine" and mentions `minijinja`)
- Verify `cargo test && cargo clippy -- -D warnings` passes

**Done when:** Dogfood config uses at least one Tera feature (conditional or pipe). All tests and clippy pass.

**Session 1 exit criteria:** 155+ tests (138 existing + ~17 new). Tera-powered templates with conditionals and pipes work. `--config` and `--timeout` flags functional. New config fields wired through. Design spec updated.

---

## Session 2: Feature Parity — P1 Config, CLI, and Stage Completeness

**Depends on:** Session 1 complete (Tera engine, new config fields, CLI infrastructure).

**Why second:** These are the features users will notice are missing. All tasks are independent of each other — ideal for subagent parallelism. Each task touches a different file or crate.

### Task 2A: CLI completeness
- Add `completion` command (clap has built-in `clap_complete` support for bash/zsh/fish/powershell)
- Add `healthcheck` command (extract env checks from `check` into a dedicated command)
- Add `--parallelism` / `-p` flag to `release` and `build` (concurrent builds across targets)
- Add `--auto-snapshot` flag (auto-set `--snapshot` if repo is dirty)
- Add `--single-target` flag (build only for host target triple)
- Add `--release-notes` flag (path to custom release notes file, overrides changelog)

**Done when:** Each new command/flag appears in `--help` output. `completion bash` produces valid bash completions. `--single-target` builds only one binary. Test for each flag.

### Task 2B: Archive stage enhancements
- Add `tar.xz` format support (add `xz2` or `liblzma` crate)
- Add `tar.zst` format support (add `zstd` crate)
- Add `binary` format (raw binary copy, no archiving)
- Add glob pattern support for `files` field (use `glob` crate)
- Add `wrap_in_directory` option

**Done when:** Tests create and verify contents of tar.xz, tar.zst, and binary-format outputs. Glob patterns like `LICENSE*` resolve correctly. `wrap_in_directory` wraps archive contents in a named directory.

### Task 2C: Release stage enhancements
- Add `header` / `footer` fields to `ReleaseConfig` (prepend/append to release body, distinct from changelog header/footer)
- Add `extra_files` field (upload additional files as release assets)
- Add `skip_upload` field
- Wire `make_latest` to GitHub API call (config field already added in Task 1F)
- Add `replace_existing_draft` and `replace_existing_artifacts`

**Done when:** Release body includes header/footer around changelog. Extra files appear in upload list. `skip_upload` prevents asset upload. Tests for each field.

### Task 2D: Docker stage enhancements
- Add `skip_push` field
- Add `extra_files` field (additional files in build context)
- Add `push_flags` field

**Done when:** `skip_push: true` skips the push command. Extra files are copied into the staging directory. Tests for each field.

### Task 2E: Sign stage — structural migration + enhancements
- **Critical:** Convert `sign` (single config) to `signs[]` (array) in config schema. Must handle backward compat: accept both `sign:` (single object, auto-wrapped into array) and `signs:` (explicit array).
- Add `id`, `ids` filter, `signature` template, `stdin`/`stdin_file` fields
- Add more `artifacts` filter values: `source`, `archive`, `binary`, `package`
- Update dogfood config and design spec schema to reflect `signs[]`

**Done when:** Both `sign:` (single) and `signs:` (array) parse correctly. Multiple sign configs with different `artifacts` filters work. Tests for migration compat and new fields.

### Task 2F: NFpm stage enhancements
- Add `scripts` block: `preinstall`, `postinstall`, `preremove`, `postremove`
- Add `recommends`, `suggests`, `conflicts`, `replaces`, `provides` fields
- Add `contents[].type` and `contents[].file_info`

**Done when:** Generated nfpm.yaml includes scripts and new package metadata. Tests verify nfpm.yaml output format.

### Task 2G: Checksum stage enhancements
- Add algorithms: `sha1`, `sha384`, `sha224`, `blake2b`, `blake2s` (add `blake2` crate)
- Add `extra_files` and `ids` filter fields

**Done when:** Each new algorithm produces correct output verified against known test vectors. Extra files appear in checksum file.

### Task 2H: Metadata, reporting, and environment
- Add `report_sizes: bool` top-level config — print artifact size table after pipeline
- Add metadata.json output to `dist/` — serialize artifact registry to JSON at pipeline end
- Add `env` top-level field (global environment variables in `KEY=VALUE` format, available to templates)

**Done when:** `report_sizes: true` prints a formatted size table. `dist/metadata.json` contains valid JSON with all artifacts. `env` vars are accessible in templates.

### Task 2I: Changelog enhancements
- Add `filters.include` (include-only patterns, complement to `exclude`)
- Add `use: github-native` support (delegate to GitHub's auto-generated release notes)
- Add `abbrev` field (hash abbreviation length)

**Done when:** `filters.include` restricts commits to matching patterns. `abbrev: 7` truncates hashes. Tests for each feature.

### Task 2J: Custom publishers
- Add `publishers[]` top-level config — generic publish mechanism
- Support command mode: `cmd` field with templated args (e.g., `curl -F 'file=@{{ ArtifactPath }}' ...`)
- Support artifact filtering by `ids` and artifact type
- This is the extensibility escape hatch — eliminates the need for dedicated integrations with every upload target

**Done when:** A publisher config with a `cmd` field executes for matching artifacts in non-dry-run mode. Dry-run logs the command without executing. Tests for command construction, artifact filtering, and dry-run behavior.

**Session 2 exit criteria:** 185+ total tests. Each new config field has at least one parsing test and one behavior test. New CLI flags appear in `--help`. `cargo clippy` clean. All new features exercised by at least one test.

---

## Session 3: Release Readiness — Tests, Docs, Publish Prep

**Depends on:** Sessions 1 and 2 complete.

**Why third:** Can't publish without confidence. Tests validate everything built in Sessions 1-2. Docs make it adoptable.

### Task 3A: E2E test infrastructure
- Create a test fixture: a minimal Cargo project with `Cargo.toml` and `src/main.rs`
- Create a workspace test fixture: multi-crate project exercising `depends_on`, per-crate tags, workspace-aware change detection
- E2E test: `anodize release --snapshot` produces correct artifacts in `dist/`
- E2E test: `anodize release --dry-run` runs full pipeline with no side effects
- E2E test: `anodize check` validates the fixture's config
- E2E test: `anodize init` generates valid config from the fixture
- E2E test: multi-crate workspace with `--all` flag detects correct changed crates

**Done when:** E2E tests run against real Cargo projects in temp dirs. Snapshot produces archives and checksums. Workspace test exercises dependency ordering.

### Task 3B: Error path tests
- Config parsing: malformed YAML, unknown fields, type mismatches, missing required fields
- Build stage: missing cargo, invalid target triple, failed compilation
- Template engine: undefined variables, syntax errors, invalid pipes
- Release stage: missing GITHUB_TOKEN, API errors
- Every stage: test the `--skip` and `disable` behavior

**Done when:** At least 15 new error path tests. Each error produces a clear, actionable error message.

### Task 3C: Stage integration tests
- Archive: real file trees, verify tar.gz/zip/tar.xz contents
- Checksum: verify checksum file format and correctness
- Changelog: verify output with real git history
- Publish (Homebrew): verify generated formula matches expected format
- Publish (Scoop): verify generated manifest

**Done when:** Integration tests create real artifacts and verify their contents byte-by-byte or structurally.

### Task 3D: Documentation
- Update `docs/configuration.md` with all new fields from Sessions 1-2
- Create `docs/templates.md` — template variable reference and Tera function list
- Create `docs/migration-from-goreleaser.md` — config translation guide
- Update `README.md` with any new CLI flags and features
- Add contribution guidelines (`CONTRIBUTING.md`) and issue templates

**Done when:** Every config field and CLI flag is documented. Migration guide covers the 10 most common GoReleaser config patterns.

### Task 3E: CI pipeline and publish prep
- Verify all `Cargo.toml` metadata is correct (description, repository, license, keywords, categories)
- Run `cargo publish --dry-run` for each crate in dependency order
- Set up CI pipeline (GitHub Actions workflow for test + clippy + fmt)
- Cross-platform CI matrix (Linux + macOS at minimum; Windows if feasible)
- Address any `serde_yaml` deprecation warnings if present
- Verify the dogfood `.anodize.yaml` config works end-to-end with `--snapshot`

**Done when:** `cargo publish --dry-run` succeeds for all crates. CI workflow runs and passes. Dogfood snapshot release produces expected output.

### Task 3F: Documentation site (can be deferred to post-publish)
- Set up docs site with mdBook or Zola
- Getting started guide, per-stage documentation, CI/CD integration guides, FAQ
- Deploy to GitHub Pages

**Done when:** Docs site builds and deploys. All major features have a dedicated page.

**Session 3 exit criteria:** 220+ tests including workspace-aware E2E and error paths. Cross-platform CI green. All features documented. `cargo publish --dry-run` succeeds. Ready for `cargo publish`.

---

## Session 4: Test Parity — Close the Gap with GoReleaser

**Depends on:** Session 3 complete (all tests passing, docs written, CI green).

**Why before audit/publish:** GoReleaser has thousands of tests covering every config field, stage, edge case, and error path. Anodize has ~436. Publishing with shallow test coverage means bugs ship to users. This session systematically identifies and closes the gap by category.

### Task 4A: Audit test parity gap
- Clone or browse GoReleaser's test suite (https://github.com/goreleaser/goreleaser) to understand their coverage strategy per stage
- For each anodize stage/module, compare:
  - **Config parsing tests:** How many config field variations does GoReleaser test per stage vs anodize? (GoReleaser typically tests: valid value, invalid value, zero value, default value, interaction with other fields — ~5-10 cases per field)
  - **Stage behavior tests:** Does anodize test each config field's effect on stage output, or just that the field parses?
  - **Error path tests:** Does each stage have tests for every error condition (missing tools, invalid input, API failures, permission errors)?
  - **E2E tests:** Does anodize have snapshot/dry-run E2E tests that exercise real builds end-to-end?
- Produce a gap matrix: `| Stage | Config parsing | Behavior | Error paths | E2E | GoReleaser approx | Anodize current | Delta |`

**Done when:** Gap matrix produced with specific counts per stage. Every "delta" cell has a concrete list of missing test cases.

### Task 4B: Config parsing depth — every field, every variation
For EVERY config field across all stages, add tests for:
- Valid value (happy path)
- Default value (field omitted)
- Invalid type (string where int expected, etc.)
- Edge cases (empty string, empty array, null/None)
- Interaction with related fields (e.g., `disable: true` + other fields set)

Priority order (by user impact):
1. `crates/core/src/config.rs` — top-level and per-crate config fields
2. Release config fields (`make_latest`, `extra_files`, `skip_upload`, `replace_existing_*`)
3. Archive config fields (`wrap_in_directory`, `format_overrides`, glob `files`)
4. Sign config fields (`signs[]` array, backward compat, `ids` filter, `stdin`/`stdin_file`)
5. Changelog, checksum, docker, nfpm, publish, announce config fields

**Done when:** Every config field has at least 3 test cases (valid, default, invalid). Fields with complex behavior have 5+.

### Task 4C: Stage behavior tests — config fields actually do things
For each stage, verify that config fields produce the correct output:
- Archive: `wrap_in_directory` actually wraps, `format_overrides` actually switches format, glob `files` resolves correctly, `binary` format copies raw file
- Checksum: each algorithm produces correct hash (verify against known test vectors), `extra_files` appear in output, `ids` filter works, `disable` skips stage
- Changelog: `header`/`footer` appear in output, `filters.include` restricts commits, `abbrev` truncates hashes, `disable` skips stage, `use: github-native` delegates correctly
- Release: `header`/`footer` in release body, `extra_files` in upload list, `skip_upload` prevents upload, `make_latest` value passed to API, `replace_existing_draft` finds and updates existing draft
- Sign: multiple sign configs each run independently, `artifacts` filter selects correct artifacts, `ids` filter works, `signature` template resolves, `stdin`/`stdin_file` pipe correctly
- Docker: `skip_push` prevents push, `extra_files` copied to staging dir, `push_flags` appended to command
- NFpm: `scripts` block appears in generated config, `recommends`/`suggests`/`conflicts`/`replaces`/`provides` all appear, `contents[].type` and `file_info` serialize correctly
- Publish: Homebrew formula format correct with multi-arch, Scoop manifest structure correct, publishers `cmd` templates resolve with artifact vars, dry-run logs without executing

**Done when:** Each config field that changes stage output has a dedicated test verifying the output change. Not just "it parses" but "it does what it says."

### Task 4D: Error path completeness
For each stage, add tests for every error condition:
- **Build:** missing cargo binary, invalid target triple, compilation failure (bad source), `copy_from` referencing nonexistent binary, timeout exceeded
- **Archive:** missing binary artifact, empty file list, invalid format string, write permission denied
- **Checksum:** missing archive artifacts, unsupported algorithm string, write failure
- **Changelog:** no git history, no previous tag, invalid regex in filters
- **Release:** missing GITHUB_TOKEN, API 401/403/404/422 errors, upload failure, network timeout
- **Sign:** missing gpg/cosign binary, signing command failure (nonzero exit), missing artifact for signing
- **Docker:** missing docker/buildx, build failure, push failure, missing Dockerfile
- **NFpm:** missing nfpm binary, invalid format, missing required fields
- **Publish:** crates.io publish failure, Homebrew tap clone failure, Scoop bucket write failure
- **Template:** undefined variable, syntax error, invalid filter name, unclosed block
- **Config:** circular `depends_on`, duplicate crate names, invalid `tag_template`, referencing nonexistent crate path

**Done when:** Every stage has error path tests for at least its 3 most likely failure modes. Error messages are verified to be clear and actionable (not just "an error occurred").

### Task 4E: E2E pipeline tests
Expand E2E coverage beyond the current 6 tests:
- **Multi-format archive:** config with `tar.gz`, `tar.xz`, `zip`, and `binary` format — verify all four produced correctly
- **Multi-sign:** two sign configs with different `artifacts` filters — verify each signs the correct subset
- **Changelog with groups:** real git history with feat/fix/chore commits — verify grouped output
- **Config validation round-trip:** `init` generates config → `check` validates it → `build --snapshot` succeeds
- **Workspace dependency ordering:** crate A depends on B — verify B builds before A, and `--all` detects changes in both
- **Skip stages:** `--skip=archive,checksum` produces binaries but no archives or checksums
- **Custom publishers:** publisher config with `cmd` and artifact filtering — verify command construction in dry-run
- **Docker staging:** verify the staging directory structure (`binaries/amd64/`, `binaries/arm64/`, Dockerfile copied)
- **Cross-platform archives:** verify format_overrides (windows → zip, linux → tar.gz) applied per target

**Done when:** 15+ E2E tests covering the major pipeline variations. Each test exercises real file I/O and verifies artifact contents structurally.

### Task 4F: Test infrastructure improvements
- **Shared test helpers:** Extract common fixture creation, git repo setup, config building into a shared test utilities module (avoid duplication across test files)
- **Mock GitHub API:** Create a lightweight mock for octocrab/GitHub API calls so release stage tests can verify API call parameters without network access
- **Test coverage report:** Run `cargo tarpaulin` or `cargo llvm-cov` to identify untested code paths — use as input for targeted test additions
- **Cross-platform CI matrix:** Ensure tests run on Linux + macOS in CI (Windows if feasible)

**Done when:** Shared test helpers exist and are used by 3+ test files. Mock GitHub API enables release stage unit tests. Coverage report generated.

**Session 4 exit criteria:** 800+ tests. Every config field has parsing + behavior tests. Every stage has error path tests. 15+ E2E tests. Coverage report shows no major untested code paths. Test infrastructure supports efficient test development going forward.

---

## Session 5: Full Audit — Code Quality Gate

**Depends on:** Session 4 complete (test parity gap closed, 800+ tests passing).

**Why before publish:** Sessions 1-4 were built fast across many parallel agents. Code quality, consistency, and dead code accumulate. A systematic audit before publishing catches design drift, duplication, unwired features, and cohesion issues that individual task reviews miss. The comprehensive test suite from Session 4 provides a safety net for refactoring.

### Task 5A: Run `/full-audit`
- Run the full-audit skill which dispatches three parallel agents:
  - **Design + Cohesion review** — inconsistent error handling, parameter styles, logging, naming conventions, cohesion issues across all 12 crates
  - **Duplication scan** — duplicated logic across stage crates, shared code that should be in `core`
  - **Gap analysis** — config fields parsed but never consumed, public functions with no production callers, error variants never constructed
- All automated checks (fmt, clippy, test) must pass before and after

### Task 5B: Fix all Round 1 findings
- Create task list from aggregated findings
- Fix all findings in priority order (critical → important → minor)
- Run test suite after each logical group of changes

### Task 5C: Round 2 verification
- Re-run full-audit to catch regressions and issues missed in Round 1
- Fix any new findings
- Continue until a round returns zero findings or 3 rounds complete

**Done when:** Full audit returns zero findings. All automated checks pass. No dead code, no duplication, no design inconsistencies across crates.

**Session 5 exit criteria:** Clean full-audit (zero findings across all three scopes). `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test --workspace` all pass. Codebase is publish-ready.

---

## Post-Publish (requires anodize on crates.io)

These cannot start until anodize is published and installable:

### Full-featured GitHub Action (separate repo: `tj-smith47/anodize-action`)
- TypeScript action with `@actions/tool-cache` for binary caching
- Structured outputs (artifacts, metadata JSON)
- Grouped log output via `@actions/core`
- Semver version constraints (`~> v0.1`)
- Cross-platform runner support

### cfgd Migration — First Real-World Adoption
- Write `.anodize.yaml` for cfgd (multi-crate, multi-docker, homebrew, krew, crates.io with ordering)
- Identify cfgd features needing new anodize capabilities (Helm, Krew, Crossplane, OLM → `after` hooks or new stages)
- Evaluate Cargo.toml version sync as first-class feature (`version_from: tag`)
- Replace cfgd's 633-line release workflow with `uses: tj-smith47/anodize@v1`
- Add cfgd as showcase in README

### Community Adoption — Popular Repo PRs
Target repos: ripgrep, bat, starship, nushell, zoxide, tokio, serde, clap
- Survey release workflows, identify pain points
- Submit PRs converting workflows to `.anodize.yaml`

---

## Release 2 — Future Features

### Rust-Specific First-Class Features (brainstorm — scope TBD)
Evaluate which of these are must-have vs nice-to-have based on what popular Rust projects actually need (informed by community adoption work). Ideas to explore:
- `cargo-binstall` metadata generation
- `rust-toolchain.toml` awareness / MSRV checking
- Workspace dependency version sync
- `cargo-dist` migration path
- Conditional compilation features (release builds with specific feature flag combos)
- `cdylib` / `staticlib` / `wasm32` target support
- Crate documentation builds (docs.rs-compatible)

### Built-in Auto-Tagging (brainstorm — semantics TBD)
Eliminate the need for a separate tagging action (like `anothrNick/github-tag-action`). No other release tool does this natively. Semantics to be designed in a dedicated session.

### GoReleaser Pro Features (free in anodize)
- Monorepo support (multiple independent workspaces)
- Nightly builds (`--nightly`)
- Config includes/templates
- Split/merge (fan out builds in CI)
- Snapcraft, dmg, msi, pkg packaging
- Chocolatey, Winget
- Reproducible builds (`SOURCE_DATE_EPOCH`)
- macOS Universal Binaries

### New Capabilities and Tracked P2 Gaps
- Blob storage upload (S3/GCS/Azure)
- AUR, Krew, source archives, SBOM generation
- Additional announce providers (Telegram, Teams, Mattermost, etc.)
- `jsonschema` command (output JSON schema for IDE support)
- `.env` file loading / `env_files` for token paths
- Config schema versioning (`version: 2`)
- Build `ignore` list (exclude specific target combos)
- Build per-target overrides
- UPX binary compression

### Maintenance
- Migrate from `serde_yaml` (deprecated) to `serde_yml` or alternative
- Keep dependencies updated (octocrab, reqwest, clap)
- Dogfood: CI pipeline for anodize itself using anodize
