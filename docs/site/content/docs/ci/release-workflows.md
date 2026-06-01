+++
title = "Release Workflow Strategies"
description = "Pick the right workflow shape for your repo: single-crate, lockstep workspace, per-crate workspace, hybrid groupings, or split-CI governance."
weight = 5
template = "docs.html"
+++

Read this page when you have a Rust workspace (single or multi-crate) and need to wire GitHub Actions release jobs. It covers the decision tree, the CLI building blocks, six copy-pasteable canonical strategies, and the anti-patterns that make workspace releases racy or wasteful.

## Decision tree

```
single crate                                     → Strategy A
workspace with [workspace.package].version        → Strategy B  (lockstep)
workspace with per-crate [package].version
  ├─ all crates release together                  → Strategy C1 (batched)
  ├─ subset lockstep, others independent          → Strategy C-hybrid
  └─ each crate releases on its own cadence       → Strategy C3 (fan-out)
add governance or secrets boundary                → wrap any of above with D
```

When `.anodizer.yaml` contains a non-empty `workspaces:` block, that wins over `[workspace.package].version` — it is the authoritative signal for per-crate-with-grouping intent.

## Building blocks

| Command | Detects from | Emits |
|---------|--------------|-------|
| `anodizer tag` | Cargo.toml shape + `.anodizer.yaml` | bump commit + per-crate tags; step outputs `crates` (JSON array) and `versions` (JSON object: crate→version) |
| `anodizer release` | tags at HEAD (or preserved-dist subdirs) | topo-ordered publish across all tagged crates |
| `anodizer release --crate X` | explicit override | single-crate publish |
| `anodizer release --preserve-dist` | — | hermetic dist tree; per-crate subdir when `--crate` is also set |
| `anodizer release --publish-only` | preserved-dist `context.json` (flat or per-crate subdirs) | consume existing dist, publish in topo order |

`anodizer tag` detects which crates have changed since their last tag, bumps versions, creates per-crate tags in one commit, and pushes everything atomically. The `crates` step output (a JSON array of crate names) lets downstream jobs skip entirely when nothing changed and drive matrix entries when something did.

## Canonical strategies

### Strategy A — Single crate

**Use when:** one `Cargo.toml` at the root, no workspace.

```yaml
name: Release

on:
  workflow_run:
    workflows: [CI]
    types: [completed]
    branches: [master]
  workflow_dispatch:

concurrency:
  group: release-${{ github.repository }}
  cancel-in-progress: false

permissions:
  contents: write

jobs:
  tag:
    if: >-
      github.event_name == 'workflow_dispatch' ||
      github.event.workflow_run.conclusion == 'success'
    runs-on: ubuntu-latest
    outputs:
      crates: ${{ steps.t.outputs.crates }}
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
          token: ${{ secrets.GH_PAT }}
      - name: Configure git identity
        run: |
          git config user.name  "github-actions[bot]"
          git config user.email "github-actions[bot]@users.noreply.github.com"
      - uses: tj-smith47/anodizer-action@v1
        id: t
        with:
          args: tag
        env:
          GITHUB_TOKEN: ${{ secrets.GH_PAT }}

  release:
    needs: tag
    if: needs.tag.outputs.crates != '[]'
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
      - uses: tj-smith47/anodizer-action@v1
        id: release
        with:
          auto-install: true
          gpg-private-key: ${{ secrets.GPG_PRIVATE_KEY }}
          args: release --clean
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          GPG_FINGERPRINT: ${{ secrets.GPG_FINGERPRINT }}
      - name: Rollback on release failure
        if: (failure() || cancelled()) && steps.release.outcome != 'skipped'
        env:
          GH_TOKEN: ${{ secrets.GH_PAT }}
          GITHUB_TOKEN: ${{ secrets.GH_PAT }}
        run: anodizer tag rollback "$GITHUB_SHA"
```

**Concurrency:** `group: release-${{ github.repository }}` — serializes the one run; `cancel-in-progress: false` so a release in flight is never killed.

**Resource cost:**

| runs | chk | rc | tc | det | sig | pub |
|------|-----|----|----|-----|-----|-----|
| 1 | 1 | 1 | 1 | optional | 1 | 1 |

**Race situation:** none — single run, single crate.

---

### Strategy B — Lockstep workspace

**Use when:** all crates in the workspace share a version via `[workspace.package].version`.

`anodizer tag` bumps the shared version, creates one workspace tag, and `anodizer release` walks all crates in topo order.

```yaml
name: Release

on:
  workflow_run:
    workflows: [CI]
    types: [completed]
    branches: [master]
  workflow_dispatch:

concurrency:
  group: release-${{ github.repository }}
  cancel-in-progress: false

permissions:
  contents: write
  packages: write

jobs:
  tag:
    if: >-
      github.event_name == 'workflow_dispatch' ||
      github.event.workflow_run.conclusion == 'success'
    runs-on: ubuntu-latest
    outputs:
      crates: ${{ steps.t.outputs.crates }}
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
          token: ${{ secrets.GH_PAT }}
      - name: Configure git identity
        run: |
          git config user.name  "github-actions[bot]"
          git config user.email "github-actions[bot]@users.noreply.github.com"
      - uses: tj-smith47/anodizer-action@v1
        id: t
        with:
          args: tag
        env:
          GITHUB_TOKEN: ${{ secrets.GH_PAT }}

  release:
    needs: tag
    if: needs.tag.outputs.crates != '[]'
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
      - uses: tj-smith47/anodizer-action@v1
        id: release
        with:
          auto-install: true
          gpg-private-key: ${{ secrets.GPG_PRIVATE_KEY }}
          args: release --clean
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          GPG_FINGERPRINT: ${{ secrets.GPG_FINGERPRINT }}
          CARGO_REGISTRY_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }}
      - name: Rollback on release failure
        if: (failure() || cancelled()) && steps.release.outcome != 'skipped'
        env:
          GH_TOKEN: ${{ secrets.GH_PAT }}
          GITHUB_TOKEN: ${{ secrets.GH_PAT }}
        run: anodizer tag rollback "$GITHUB_SHA"
```

**Concurrency:** same group key as A — one live run per repo.

**Resource cost:**

| runs | chk | rc | tc | det | sig | pub |
|------|-----|----|----|-----|-----|-----|
| 1 | 1 | 1 | 1 | optional | 1 | N crates (sequential) |

**Race situation:** none — `anodizer release` publishes crates in topo order; the build wall-clock of crate N+1 covers the crates.io index propagation window for crate N.

---

### Strategy C1 — Per-crate workspace, batched release

**Use when:** crates have independent versions but always release together. The determinism harness runs per-crate in a matrix (so shards build only the targets relevant to each crate); a single `release --publish-only` job consumes the preserved-dist subdirs and publishes in topo order.

```yaml
name: Release

on:
  workflow_run:
    workflows: [CI]
    types: [completed]
    branches: [master]
  workflow_dispatch:

concurrency:
  group: release-${{ github.repository }}
  cancel-in-progress: false

permissions:
  contents: write
  packages: write

jobs:
  tag:
    if: >-
      github.event_name == 'workflow_dispatch' ||
      github.event.workflow_run.conclusion == 'success'
    runs-on: ubuntu-latest
    outputs:
      crates: ${{ steps.t.outputs.crates }}
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
          token: ${{ secrets.GH_PAT }}
      - name: Configure git identity
        run: |
          git config user.name  "github-actions[bot]"
          git config user.email "github-actions[bot]@users.noreply.github.com"
      - uses: tj-smith47/anodizer-action@v1
        id: t
        with:
          args: tag
        env:
          GITHUB_TOKEN: ${{ secrets.GH_PAT }}

  determinism-check:
    needs: tag
    if: needs.tag.outputs.crates != '[]'
    strategy:
      fail-fast: false
      matrix:
        crate: ${{ fromJson(needs.tag.outputs.crates) }}
        shard: [linux, macos, windows-x86_64, windows-aarch64]
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
      - uses: tj-smith47/anodizer-action@v1
        with:
          determinism: true
          preserve-dist: "true"
          shard-label: ${{ matrix.crate }}-${{ matrix.shard }}
          determinism-crate: ${{ matrix.crate }}
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}

  release:
    needs: [tag, determinism-check]
    if: needs.tag.outputs.crates != '[]'
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
      - uses: tj-smith47/anodizer-action@v1
        id: release
        with:
          auto-install: true
          download-dist: true
          gpg-private-key: ${{ secrets.GPG_PRIVATE_KEY }}
          args: release --publish-only
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          GPG_FINGERPRINT: ${{ secrets.GPG_FINGERPRINT }}
          CARGO_REGISTRY_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }}
      - name: Rollback on release failure
        if: (failure() || cancelled()) && steps.release.outcome != 'skipped'
        env:
          GH_TOKEN: ${{ secrets.GH_PAT }}
          GITHUB_TOKEN: ${{ secrets.GH_PAT }}
        run: anodizer tag rollback "$GITHUB_SHA"
```

**Concurrency:** one live run per repo; the determinism matrix runs in parallel within the run.

**Resource cost:**

| runs | chk | rc | tc | det | sig | pub |
|------|-----|----|----|-----|-----|-----|
| 1 | 1 | 1 | 1 | N crates × 4 shards | 1 | N crates (sequential) |

**Race situation:** none — all crates build deterministically under one run; `--publish-only` iterates in topo order.

---

### Strategy C-hybrid — Multiple workspace groups

**Use when:** `.anodizer.yaml` has a `workspaces:` block defining named groups. Each group behaves like a mini-lockstep workspace; `anodizer tag` handles all groups in one invocation and one atomic push. The `crates` output lists every crate that received a new tag, regardless of which group it belongs to.

```yaml
# .anodizer.yaml (excerpt)
workspaces:
  core-group:
    crates: [myproj-core, myproj-macros]
  bin-group:
    crates: [myproj-bin-a, myproj-bin-b]
  standalone:
    crates: [myproj-cli]
```

```yaml
name: Release

on:
  workflow_run:
    workflows: [CI]
    types: [completed]
    branches: [master]
  workflow_dispatch:

concurrency:
  group: release-${{ github.repository }}
  cancel-in-progress: false

permissions:
  contents: write
  packages: write

jobs:
  tag:
    if: >-
      github.event_name == 'workflow_dispatch' ||
      github.event.workflow_run.conclusion == 'success'
    runs-on: ubuntu-latest
    outputs:
      crates: ${{ steps.t.outputs.crates }}
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
          token: ${{ secrets.GH_PAT }}
      - name: Configure git identity
        run: |
          git config user.name  "github-actions[bot]"
          git config user.email "github-actions[bot]@users.noreply.github.com"
      - uses: tj-smith47/anodizer-action@v1
        id: t
        with:
          args: tag
        env:
          GITHUB_TOKEN: ${{ secrets.GH_PAT }}

  determinism-check:
    needs: tag
    if: needs.tag.outputs.crates != '[]'
    strategy:
      fail-fast: false
      matrix:
        crate: ${{ fromJson(needs.tag.outputs.crates) }}
        shard: [linux, macos, windows-x86_64, windows-aarch64]
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
      - uses: tj-smith47/anodizer-action@v1
        with:
          determinism: true
          preserve-dist: "true"
          shard-label: ${{ matrix.crate }}-${{ matrix.shard }}
          determinism-crate: ${{ matrix.crate }}
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}

  release:
    needs: [tag, determinism-check]
    if: needs.tag.outputs.crates != '[]'
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
      - uses: tj-smith47/anodizer-action@v1
        id: release
        with:
          auto-install: true
          download-dist: true
          gpg-private-key: ${{ secrets.GPG_PRIVATE_KEY }}
          args: release --publish-only
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          GPG_FINGERPRINT: ${{ secrets.GPG_FINGERPRINT }}
          CARGO_REGISTRY_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }}
      - name: Rollback on release failure
        if: (failure() || cancelled()) && steps.release.outcome != 'skipped'
        env:
          GH_TOKEN: ${{ secrets.GH_PAT }}
          GITHUB_TOKEN: ${{ secrets.GH_PAT }}
        run: anodizer tag rollback "$GITHUB_SHA"
```

**Concurrency:** one live run per repo.

**Resource cost:**

| runs | chk | rc | tc | det | sig | pub |
|------|-----|----|----|-----|-----|-----|
| 1 | 1 | 1 | 1 | tagged-crates × 4 shards | 1 | tagged crates (topo) |

**Race situation:** none — groups are resolved inside `anodizer tag` before any push; inter-group topo order is preserved by `--publish-only`.

---

### Strategy C3 — Per-crate fan-out (independent cadences)

**Use when:** crates release on genuinely independent schedules and have no cross-crate `depends_on` relationships. Each crate's tag triggers its own release run.

The workspace-level concurrency group serializes concurrent runs so a simultaneous `core-v1.1.0` and `cli-v2.3.0` push does not race at the runner level.

```yaml
name: Release

on:
  push:
    tags:
      - "myproj-core-v*"
      - "myproj-bin-a-v*"
      - "myproj-bin-b-v*"
  workflow_dispatch:
    inputs:
      tag:
        description: "Tag to release (e.g. myproj-core-v1.2.3)"
        required: true

concurrency:
  # Serialize all release runs repo-wide; never key on the tag ref.
  group: release-${{ github.repository }}
  cancel-in-progress: false

permissions:
  contents: write
  packages: write

jobs:
  resolve:
    runs-on: ubuntu-latest
    outputs:
      crate: ${{ steps.r.outputs.workspace }}
      has-builds: ${{ steps.r.outputs.has-builds }}
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
      - uses: tj-smith47/anodizer-action@v1
        id: r
        with:
          resolve-workspace: true
          install-only: true

  release:
    needs: resolve
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
      - uses: tj-smith47/anodizer-action@v1
        id: release
        with:
          auto-install: true
          gpg-private-key: ${{ secrets.GPG_PRIVATE_KEY }}
          args: release --crate ${{ needs.resolve.outputs.crate }} --clean
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          GPG_FINGERPRINT: ${{ secrets.GPG_FINGERPRINT }}
          CARGO_REGISTRY_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }}
      - name: Rollback on release failure
        if: (failure() || cancelled()) && steps.release.outcome != 'skipped'
        env:
          GH_TOKEN: ${{ secrets.GH_PAT }}
          GITHUB_TOKEN: ${{ secrets.GH_PAT }}
        run: anodizer tag rollback "$GITHUB_SHA"
```

**Concurrency:** `group: release-${{ github.repository }}` — the repo-wide key serializes concurrent tag pushes. Do not use `group: release-${{ github.ref_name }}`; that creates one group per tag ref, allowing parallel runs that race at the registry.

**Resource cost:**

| runs | chk | rc | tc | det | sig | pub |
|------|-----|----|----|-----|-----|-----|
| 1 per tag | 1 | 1 | 1 | 1 | 1 | 1 |

**Race situation:** minimized by the repo-wide concurrency group, but simultaneous pushes of interdependent crates can still publish in the wrong order if `cancel-in-progress` is false and run order isn't controlled. C1 or C-hybrid are safer for crates that share a `depends_on`.

---

### Strategy D — Split CI → Release (governance / secrets)

**Use when:** release secrets (signing keys, registry tokens, approval environments) must live in a separate workflow from CI, or you need a manual-approval gate before publish. Wraps any of A/B/C1/C-hybrid.

The CI workflow triggers the release workflow via `workflow_run` and passes the `crates` output through a job output. The release workflow validates that CI succeeded before tagging.

```yaml
# ci.yml
name: CI

on:
  push:
    branches: [master]
  pull_request:

jobs:
  build-test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - run: cargo test --workspace
      - run: cargo clippy --workspace -- -D warnings

  # Upload anodizer binary so release.yml can reuse it without reinstalling.
  upload-anodizer:
    needs: build-test
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v6
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - run: cargo build --release -p anodizer
      - uses: actions/upload-artifact@v4
        with:
          name: anodizer-linux
          path: target/release/anodizer
```

```yaml
# release.yml
name: Release

on:
  workflow_run:
    workflows: [CI]
    types: [completed]
    branches: [master]
  workflow_dispatch:

concurrency:
  group: release-${{ github.repository }}
  cancel-in-progress: false

permissions:
  contents: write
  packages: write

jobs:
  tag:
    if: >-
      github.event_name == 'workflow_dispatch' ||
      github.event.workflow_run.conclusion == 'success'
    runs-on: ubuntu-latest
    outputs:
      crates: ${{ steps.t.outputs.crates }}
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
          token: ${{ secrets.GH_PAT }}
      - name: Configure git identity
        run: |
          git config user.name  "github-actions[bot]"
          git config user.email "github-actions[bot]@users.noreply.github.com"
      # Reuse the binary CI already built.
      - uses: tj-smith47/anodizer-action@v1
        id: t
        with:
          from-artifact: anodizer-linux
          artifact-run-id: auto
          artifact-workflow: ci.yml
          args: tag
        env:
          GITHUB_TOKEN: ${{ secrets.GH_PAT }}

  release:
    needs: tag
    if: needs.tag.outputs.crates != '[]'
    runs-on: ubuntu-latest
    # Optional: require manual approval before this job runs.
    environment: production
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
      - uses: tj-smith47/anodizer-action@v1
        id: release
        with:
          from-artifact: anodizer-linux
          artifact-run-id: auto
          artifact-workflow: ci.yml
          auto-install: true
          gpg-private-key: ${{ secrets.GPG_PRIVATE_KEY }}
          cosign-key: ${{ secrets.COSIGN_KEY }}
          args: release --clean
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          GPG_FINGERPRINT: ${{ secrets.GPG_FINGERPRINT }}
          COSIGN_PASSWORD: ${{ secrets.COSIGN_PASSWORD }}
          CARGO_REGISTRY_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }}
      - name: Rollback on release failure
        if: (failure() || cancelled()) && steps.release.outcome != 'skipped'
        env:
          GH_TOKEN: ${{ secrets.GH_PAT }}
          GITHUB_TOKEN: ${{ secrets.GH_PAT }}
        run: anodizer tag rollback "$GITHUB_SHA"
```

**Concurrency:** repo-wide group in `release.yml`; CI can fan out freely.

**Resource cost:**

| runs | chk | rc | tc | det | sig | pub |
|------|-----|----|----|-----|-----|-----|
| 2 workflows | CI + release | shared (artifact) | 1 | optional | 1 | N crates |

**Race situation:** none — `workflow_run` fires once per CI completion; the `crates != '[]'` gate skips the release job when nothing changed.

---

## Concurrency primer

| Group key | `cancel-in-progress` | Use with |
|-----------|---------------------|----------|
| `release-${{ github.repository }}` | `false` | All release strategies — serializes runs repo-wide; a release in flight is never killed |
| `ci-${{ github.ref }}` | `true` | CI workflows — cancels stale pushes to the same branch so the latest commit always runs |
| `release-${{ github.ref_name }}` | `false` | **Avoid for releases** — creates one group per tag ref, enabling parallel runs |

The `cancel-in-progress: false` requirement for release is non-negotiable: publishing to crates.io, signing artifacts, and creating GitHub Releases are partially irreversible. Killing a run mid-flight leaves artifacts in an inconsistent state.

## Permissions

Every release workflow needs at least `contents: write` (release creation +
tag mutation). Add the others as your strategy uses them:

| Permission | When |
|---|---|
| `contents: write` | Always (release creation, tag rollback, version_sync commits) |
| `actions: read` | When the release job downloads artifacts from a sibling workflow (`from-artifact: anodizer-linux` in Strategy D, the cross-workflow artifact pattern, `--publish-only` consuming preserved-dist from a prior `determinism-check` run). The `actions/download-artifact@v4` action requires it for `merge-multiple: true` cross-workflow downloads |
| `packages: write` | `docker_v2[]` (GHCR), GitHub Packages npm publishes |
| `id-token: write` | `mcp.auth.type: github-oidc`, cosign keyless, any OIDC-anchored publisher |

```yaml
permissions:
  contents: write
  actions: read          # for cross-workflow artifact downloads
  packages: write        # for ghcr.io docker_v2 pushes
  id-token: write        # for cosign keyless / mcp github-oidc
```

## Anti-patterns

**Tag-fanout concurrency keyed per-tag-ref.** `group: release-${{ github.ref_name }}` creates a separate concurrency group for every tag, allowing N parallel release runs when N tags land simultaneously. Parallel crates.io publishes of `core` and `bin` race the sparse-index propagation window.

**Leader election among parallel triggered runs.** Using a lock artifact or environment variable to elect one "winner" among N simultaneously triggered runs still pays the N× resource cost (checkout, toolchain, cache hydration) before the losers bail out.

**Polling crates.io for upstream deps.** Sleeping and retrying `cargo publish` until the upstream index entry surfaces treats the symptom (publish race) rather than the cause (fan-out). `anodizer release`'s topo-sorted sequential publish makes this unnecessary.

**Per-crate determinism jobs without a shared rust-cache key.** When the determinism matrix uses a different cache key per crate, each shard cold-compiles the full dependency tree. Pin `Swatinem/rust-cache` to the same workspace-level key across all matrix entries.

**Bash loops invoking `anodizer tag --crate X` repeatedly.** Calling `anodizer tag` once per crate in a shell loop duplicates the change-detection logic, creates one bump commit and tag push per crate (N pushes instead of 1), and can trigger N downstream release runs. `anodizer tag` without `--crate` handles the entire workspace in one atomic commit + push.

**`[workspace.package].version` set "just in case" when crates are per-crate-versioned.** Before the per-crate detection fix, setting a shared workspace version caused all crates to be treated as lockstep even when individual `[package].version` fields were present. The current detection order is: `.anodizer.yaml workspaces:` first, then `[workspace.package].version`, then per-crate `[package].version`. Setting `[workspace.package].version` on a per-crate workspace forces lockstep behavior regardless of what the individual `[package].version` fields say.

**`tag.skip_ci_on_bump: true` with an `on: push: tags:` release.** GitHub suppresses tag-push triggers when the tag target commit's message contains `[skip ci]`. Since the version-sync bump commit *is* the tag target, enabling `skip_ci_on_bump` under a tag-push-triggered release silently skips the release entirely. Only enable it with a `workflow_run`-triggered release, where the trigger is the completed CI run rather than the tag push (see below).

## `[skip ci]` on the bump commit vs. the release trigger

`anodizer tag` writes a version-sync bump commit before creating the tag. By default that commit's subject does **not** carry `[skip ci]`, because the bump commit becomes the tag target and `[skip ci]` on a tag target suppresses both the master-push CI re-run **and** any `on: push: tags:` release trigger.

| Release trigger | `tag.skip_ci_on_bump` | Effect |
|---|---|---|
| `on: push: tags:` (GoReleaser-style) | **off** (default) | The bump-commit push re-runs CI (its auto-tag job no-ops); the tag push fires the release. Marking `[skip ci]` here would kill the release. |
| `on: workflow_run:` (decoupled CI → release) | may be **on** | The release fires off CI completion, not the tag push, so `[skip ci]` only skips the redundant master-push CI re-run. |

```yaml
# .anodizer.yaml — only with a workflow_run-triggered release.yml
tag:
  skip_ci_on_bump: true
```

## Migrating from per-tag fan-out

The old pattern triggered one workflow run per tag and resolved the crate inside the run. Replace it with a `workflow_run` trigger and let `anodizer tag` emit the crate list.

**Before:**

```yaml
# release.yml (old)
on:
  push:
    tags:
      - "myproj-core-v*"
      - "myproj-bin-a-v*"
      - "myproj-bin-b-v*"

concurrency:
  group: release-${{ github.ref_name }}   # per-tag group — parallel runs allowed
  cancel-in-progress: false

jobs:
  resolve:
    runs-on: ubuntu-latest
    outputs:
      crate: ${{ steps.r.outputs.workspace }}
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
      - uses: tj-smith47/anodizer-action@v1
        id: r
        with:
          resolve-workspace: true
          install-only: true

  release:
    needs: resolve
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
      - uses: tj-smith47/anodizer-action@v1
        with:
          auto-install: true
          args: release --crate ${{ needs.resolve.outputs.crate }} --clean
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

```yaml
# ci.yml (old) — drove a bash loop
      - name: Auto-tag all workspaces
        env:
          GITHUB_TOKEN: ${{ secrets.GH_PAT }}
        run: |
          for crate in myproj-core myproj-bin-a myproj-bin-b; do
            anodizer tag --crate "$crate" || true
          done
          git push origin HEAD || true
```

**After:**

```yaml
# ci.yml — one step, no loop
      - uses: tj-smith47/anodizer-action@v1
        id: t
        with:
          args: tag
        env:
          GITHUB_TOKEN: ${{ secrets.GH_PAT }}
```

```yaml
# release.yml — workflow_run trigger, crates gate, topo publish
on:
  workflow_run:
    workflows: [CI]
    types: [completed]
    branches: [master]

concurrency:
  group: release-${{ github.repository }}  # repo-wide, no per-tag parallelism
  cancel-in-progress: false

jobs:
  tag:
    if: >-
      github.event_name == 'workflow_dispatch' ||
      github.event.workflow_run.conclusion == 'success'
    runs-on: ubuntu-latest
    outputs:
      crates: ${{ steps.t.outputs.crates }}
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
          token: ${{ secrets.GH_PAT }}
      - name: Configure git identity
        run: |
          git config user.name  "github-actions[bot]"
          git config user.email "github-actions[bot]@users.noreply.github.com"
      - uses: tj-smith47/anodizer-action@v1
        id: t
        with:
          args: tag
        env:
          GITHUB_TOKEN: ${{ secrets.GH_PAT }}

  release:
    needs: tag
    if: needs.tag.outputs.crates != '[]'
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
      - uses: tj-smith47/anodizer-action@v1
        with:
          auto-install: true
          args: release --publish-only
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          CARGO_REGISTRY_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }}
```

Key changes:
- Trigger: `on: push: tags:` → `on: workflow_run:` (one run per CI completion, not one per tag)
- Concurrency group: per-tag ref → repo-wide
- CI: bash loop → single `anodizer tag` step with `crates` output
- Release: per-crate `--crate X` → `--publish-only` (topo order from tags at HEAD)
- `resolve` job: dropped — `anodizer tag` emits everything it was computing
