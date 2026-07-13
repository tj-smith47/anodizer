+++
title = "The Release Pipeline (topology)"
description = "The hardened end-to-end shape anodizer ships with: preflight → auto-tag → determinism → publish → npm-provenance, with copy-pasteable YAML."
weight = 3
template = "docs.html"
+++

This is the production-grade release pipeline anodizer runs against itself, generalized for any consumer. It is the most hardened shape — a secret gate that runs *before a tag exists*, a commit-driven auto-tag, a sharded byte-for-byte reproducibility proof, a publish step that ships the **proven** artifacts (never a rebuild), and an npm leg split out so npm provenance can be minted from a GitHub-hosted OIDC token.

If you just want a release on tag-push, start with [GitHub Actions](@/docs/ci/github-actions.md). Reach for this topology when you publish to one-way-door registries (crates.io, chocolatey, winget, snapcraft) and want every byte proven reproducible before it ships.

## Topology at a glance

```
CI (master, success)  ──or──  workflow_dispatch
        │
        ▼
  preflight            validate every publish secret + key material BEFORE a
        │              tag exists (release --preflight-secrets). A missing CI
        │              secret aborts here — nothing is tagged.
        ▼
  tag (auto-tag)       anodizer tag --push --changelog. Reads the commit range
        │              for #major/#minor/#patch/#none + conventional markers,
        │              bumps the version, writes it back, tags, pushes atomically.
        │              Emits: tagged, sha, should_run_determinism, …
        ▼
  determinism-check    4 parallel shards (ubuntu / macos / windows-x86_64 /
        │              windows-aarch64). Each builds N times, proves byte-equal,
        │              and uploads its hermetic dist-* artifact.
        ▼
  release (publish)    download + merge all 4 shards' preserved dist →
        │              release --publish-only --skip=npm,pypi,cargo. Ships the
        │              PROVEN bytes; never recompiles. Runs every non-OIDC publisher.
        ▼
  dispatch-oidc        gh workflow run publish-oidc.yml (+ wait for its verdict).
        │              release.yml fires on workflow_run, which crates.io/PyPI
        │              Trusted Publishing REJECT; dispatch hops onto an accepted
        ▼              trigger without tainting the OIDC event_name claim.
  publish-oidc.yml     release --publish-only --publishers npm,pypi,cargo, on a
  (workflow_dispatch)  github-hosted runner so the GitHub Actions OIDC identity is
                       accepted: npm provenance + PyPI + crates.io Trusted Publishing.
```

The release job **publishes the shards' preserved dist — it never rebuilds.** An artifact ships only if a determinism shard produced it: the stage list that the shards validate is also the produce filter.

## The jobs, one at a time

### 1. `preflight` — gate secrets before tagging

Tagging is a half-irreversible act: once `vX.Y.Z` is pushed, a downstream release fires. The preflight job validates that **every** runner-agnostic publish secret and key blob the later jobs need is present and well-formed **before** the tag is minted, so a truncated `COSIGN_KEY` or a missing `CARGO_REGISTRY_TOKEN` aborts the run with nothing published and no orphan tag.

```yaml
  preflight:
    name: Preflight secrets
    if: ${{ github.event.workflow_run.conclusion == 'success' || github.event_name == 'workflow_dispatch' }}
    runs-on: ubuntu-latest
    permissions:
      contents: read
      id-token: write          # so OIDC request vars are present for the npm/mcp/pypi check
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
      - uses: tj-smith47/anodizer-action@v1
        with:
          auto-install: true
          # --skip=blob when blob creds are ambient on the publish runner,
          # not GitHub repo secrets (this gate cannot see them).
          args: release --preflight-secrets --skip=blob
        env:
          CARGO_REGISTRY_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }}
          CHOCOLATEY_API_KEY: ${{ secrets.CHOCOLATEY_API_KEY }}
          COSIGN_KEY: ${{ secrets.COSIGN_KEY }}
          COSIGN_PASSWORD: ${{ secrets.COSIGN_PASSWORD }}
          GPG_FINGERPRINT: ${{ secrets.GPG_FINGERPRINT }}
          NPM_TOKEN: ${{ secrets.NPM_TOKEN }}
          # …one line per publish secret your config references…
```

`release --preflight-secrets` validates secret presence and key-material shape **without** probing host-local tools, so it runs cleanly on a github-hosted gate even when the real publish runs elsewhere. See [Preflight](@/docs/general/preflight.md) for the full check matrix.

### 2. `tag` — commit-driven auto-tag

The tag job runs `anodizer tag --push --changelog`: it scans the commit range since the last tag, resolves a bump from commit-message directives and conventional markers, writes the new version back into `Cargo.toml` (+ enrolled `version_files`), refreshes `CHANGELOG.md`, then pushes the bump commit and the tag atomically.

```yaml
  tag:
    name: Auto-tag
    needs: [preflight]
    if: ${{ needs.preflight.result == 'success' }}
    runs-on: ubuntu-latest
    permissions:
      contents: write
    outputs:
      tagged: ${{ steps.t.outputs.tagged }}
      sha: ${{ steps.t.outputs.head-sha }}
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
          args: tag --push --changelog
        env:
          GITHUB_TOKEN: ${{ secrets.GH_PAT }}
```

The `tagged` output gates everything downstream: `'false'` (a chore/docs/ci-only or `#none` range) skips the rest of the pipeline. `head-sha` is the commit the tag points at — check **that** out in later jobs so the tree matches the tag. The consumer-level bump model is summarized [below](#the-version-bump-model-consumer-level); the full precedence table is in [Auto-Tagging](@/docs/advanced/auto-tagging.md).

### 3. `determinism-check` — 4 sharded reproducibility proofs

A reusable workflow fans the determinism harness across four shards (one per host/target family). Each shard builds the release N times, asserts every produced byte is identical across runs, and uploads its hermetic `dist-<shard>` artifact for the publish job to consume. Manifests carry a `-<shard-label>` suffix so the four uploads merge without collision.

```yaml
  determinism-check:
    name: Determinism
    needs: tag
    if: needs.tag.outputs.tagged == 'true'
    strategy:
      fail-fast: false
      matrix:
        include:
          - { shard: ubuntu-latest,    os: ubuntu-latest }
          - { shard: macos-latest,     os: macos-latest }
          - { shard: windows-x86_64,   os: windows-latest }
          - { shard: windows-aarch64,  os: windows-latest }
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
          ref: ${{ needs.tag.outputs.sha }}
      - uses: tj-smith47/anodizer-action@v1
        with:
          determinism: true
          preserve-dist: "true"     # write hermetic dist to ./preserved-dist
          shard-label: ${{ matrix.shard }}
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

`determinism: true` is the entire body of a shard — the action installs Rust, the cross-build deps, `rustup target add`s the configured triples for the shard's OS, and runs `anodizer check determinism`. See [Determinism](@/docs/advanced/determinism.md) for the harness semantics and [`preserve-dist`](@/docs/ci/anodizer-action.md#determinism-harness) for the artifact contract.

### 4. `release` — publish the proven bytes

The publish job downloads and merges all four shards' preserved dist, asserts every shard arrived, then runs `release --publish-only`. **It does not rebuild** — it publishes the byte-stable artifacts the shards already proved. `--skip=npm,pypi,cargo` peels the OIDC publishers onto the standalone workflow dispatched below.

```yaml
  release:
    name: Publish Release
    needs: [tag, determinism-check]
    # !cancelled() is load-bearing: it lets the explicit gate govern when
    # determinism-check is skipped (the re-publish path) rather than GHA
    # applying an implicit success() and skipping the publish. Exclude both
    # failure AND cancelled — a cancelled shard leaves the merged dist partial.
    if: ${{ !cancelled() && needs.tag.outputs.tagged == 'true' && needs.determinism-check.result != 'failure' && needs.determinism-check.result != 'cancelled' }}
    runs-on: ubuntu-latest
    permissions:
      contents: write
      id-token: write
      packages: write
      attestations: write
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
          ref: ${{ needs.tag.outputs.sha }}
      - uses: tj-smith47/anodizer-action@v1
        with:
          auto-install: true
          download-dist: true        # merge all dist-* shards
          gpg-private-key: ${{ secrets.GPG_PRIVATE_KEY }}
          args: release --publish-only --skip=npm,pypi,cargo
        env:
          GITHUB_TOKEN: ${{ secrets.GH_PAT }}
          GPG_FINGERPRINT: ${{ secrets.GPG_FINGERPRINT }}
          # …the same publish-secret env block the preflight gate validated…
```

There is **no workflow-side rollback step**: `anodizer release` executes the [`release.on_failure` policy](@/docs/advanced/release-resilience.md#release-on-failure-the-in-process-failure-policy) in-process — rolling back the tag and bump by default, auto-degrading to `hold` once a one-way-door publisher has landed.

### 5. `dispatch-oidc` → `publish-oidc.yml` — OIDC publishers on a hosted runner

Three publishers authenticate from a GitHub Actions OIDC identity that the registry only honours from a github-hosted runner: **npm** provenance (minted from the id-token; a self-hosted runner 422s and degrades to a non-provenance publish), **pypi** [Trusted Publishing](@/docs/publish/pypi.md), and **cargo** [crates.io Trusted Publishing](@/docs/publish/crates-io.md#trusted-publishing-oidc) (`auth: oidc` exchanges the id-token for a short-lived upload token — no stored `PYPI_TOKEN` / `CARGO_REGISTRY_TOKEN`). The main publish skips all three.

These do **not** run as a job inside `release.yml`. crates.io and PyPI Trusted Publishing accept only `push`, `release`, and `workflow_dispatch` — they **reject the `workflow_run` event** `release.yml` fires on (`400 "does not support the workflow_run event trigger"`), and the OIDC `event_name` claim is fixed per workflow-run, so no job inside `release.yml` can present an accepted trigger. So the OIDC publishers live in a standalone **`publish-oidc.yml`** (`on: workflow_dispatch`), and a small `dispatch-oidc` job triggers it via the Actions API and waits on its verdict — the release run still reflects the OIDC leg's pass/fail. A reusable `workflow_call` workflow would not work either: it inherits the caller's `workflow_run` event.

```yaml
  # In release.yml: dispatch the standalone workflow and block on its result.
  dispatch-oidc:
    name: Dispatch OIDC publish
    needs: [tag, release]
    # always() + explicit result check: gated on a real publish, not skipped-need
    # propagation. Byte-equal to advance-master's if: (audit-workflow-lockstep.sh).
    if: ${{ always() && needs.release.result == 'success' }}
    runs-on: ubuntu-latest
    permissions:
      contents: read
      actions: write        # dispatch publish-oidc.yml via the Actions API
    steps:
      - uses: actions/checkout@v7
      - uses: ./.github/actions/dispatch-and-wait
        with:
          workflow: publish-oidc.yml
          github-token: ${{ secrets.GITHUB_TOKEN }}
          inputs: |
            sha=${{ needs.tag.outputs.sha }}
            dist_run_id=${{ needs.tag.outputs.dist_run_id || github.run_id }}
```

```yaml
  # In publish-oidc.yml: on: workflow_dispatch — an accepted Trusted-Publishing trigger.
  publish-oidc:
    name: Publish OIDC (npm, PyPI, crates.io)
    runs-on: ubuntu-latest
    permissions:
      contents: read
      id-token: write
      attestations: write
    steps:
      - uses: actions/checkout@v7
        with: { ref: ${{ inputs.sha }} }
      - uses: ./.github/actions/download-preserved-dist
        with: { run-id: ${{ inputs.dist_run_id }} }
      - uses: tj-smith47/anodizer-action@v1
        with:
          auto-install: true
          # --publishers npm,pypi,cargo auto-determines the surface: it deselects
          # every publisher outside the hosted set (including github-release) and
          # self-skips the sign loops, so this runner is never asked for cosign/GPG
          # material — none of npm, pypi, cargo consumes it.
          args: release --publish-only --publishers npm,pypi,cargo
        env:
          GITHUB_TOKEN: ${{ secrets.GH_PAT }}
          NPM_TOKEN: ${{ secrets.NPM_TOKEN }}   # first-publish fallback; pypi/cargo under auth: oidc need no token
```

> **`dist_run_id` on a fresh cut.** When the tag is freshly minted, `dist_run_id` is empty and the preserved `dist-*` artifacts live under the release run's own `github.run_id` — so `release.yml` passes `dist_run_id || github.run_id`. The dispatched run has a different id and cannot fall back on its own; the caller must hand it the right run to download from.

## The version-bump model (consumer level)

The `tag` job decides whether to cut a release — and which part to bump — from the commit range since the last tag. You drive it from commit messages; nothing else is required.

**Explicit tokens** (whole-word, anywhere in any commit subject/body in the range) are operator intent and always win:

| Token | Bump | `v1.4.2` → |
|-------|------|------------|
| `#major` | major | `v2.0.0` |
| `#minor` | minor | `v1.5.0` |
| `#patch` | patch | `v1.4.3` |
| `#none` | none — no release | (skips) |

**Conventional commits** are read when no explicit token is present:

| Commit prefix | Bump |
|---------------|------|
| `feat!:` / `BREAKING CHANGE:` | major |
| `feat:` | minor |
| `fix:` / `perf:` / `revert:` | patch |
| `chore`/`docs`/`style`/`refactor`/`test`/`build`/`ci` | none |

Precedence, highest first: **explicit `#token`** → **conventional marker** → **`#none`** → **`default_bump`** (config, default `none`). A release-worthy conventional marker beats a `#none` in the same range; an explicit token is never demoted.

```bash
# These commits, since the last tag:
fix: handle empty target list        # → patch
docs: clarify retry semantics #none  # → none (but the fix above wins)
# Result: a patch bump. #none only vetoes the default fallback, not a real fix.
```

```bash
git commit -m "feat: add cloudsmith publisher #minor"   # explicit token → minor
git commit -m "chore: bump deps #none"                  # chore + #none   → no release
git commit -m "fix!: drop the legacy flag"              # conventional!   → major
```

> **Pre-1.0:** while the major version is `0`, `bump_minor_pre_major: true` demotes an *inferred* breaking change to a minor bump (stays `0.x`). Only an explicit `#major` (or a manual `Cargo.toml` bump) reaches `1.0.0`. See [Auto-Tagging → Pre-1.0 demotion](@/docs/advanced/auto-tagging.md#pre-1-0-demotion).

The full precedence table, the `Cargo.toml`-ahead guard, and every `tag:` config field live in [Auto-Tagging](@/docs/advanced/auto-tagging.md).

## Why split CI, tag, and publish?

| Concern | Where it lives | Why |
|---------|----------------|-----|
| Secret presence | `preflight` | Catch a missing/mangled secret **before** a tag exists, not halfway through publishing |
| Version decision | `tag` | One commit-driven bump + atomic push; downstream gates on `tagged` |
| Reproducibility | `determinism-check` | Prove every byte is reproducible across hosts before any of it ships |
| Publishing | `release` | Ship the **proven** bytes; in-process `on_failure` policy handles partial failure |
| OIDC publishers (npm provenance, PyPI + crates.io Trusted Publishing) | `dispatch-oidc` → `publish-oidc.yml` | crates.io/PyPI reject `workflow_run`, so a standalone `workflow_dispatch` workflow runs them on an accepted trigger + github-hosted runner |

For the lighter-weight shapes — single-crate tag-push, lockstep workspace, per-crate fan-out — see [Release Workflow Strategies](@/docs/ci/release-workflows.md), which presents a decision tree and a canonical YAML per shape.

## See also

- [GitHub Actions](@/docs/ci/github-actions.md) — quick-start release jobs
- [anodizer-action reference](@/docs/ci/anodizer-action.md) — every input and output
- [Release Workflow Strategies](@/docs/ci/release-workflows.md) — pick a shape for your repo
- [Auto-Tagging](@/docs/advanced/auto-tagging.md) — the full version-bump model
- [Determinism](@/docs/advanced/determinism.md) — the reproducibility harness
- [Preflight](@/docs/general/preflight.md) — the pre-stage environment gate
- [Release Resilience](@/docs/advanced/release-resilience.md) — the in-process `on_failure` policy
