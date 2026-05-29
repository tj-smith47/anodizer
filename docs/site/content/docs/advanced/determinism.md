+++
title = "Determinism"
description = "Byte-stability contract, allow-list, and the anodize check determinism harness"
weight = 7
template = "docs.html"
+++

Anodizer's broader correctness story depends on consumers being able to
independently verify that the artifacts on a release match the bytes a clean
rebuild from the same commit would produce. Without that, SHA256SUMS in a
release body is informational only; a sophisticated consumer cannot tell a
corrupted upload from an expected build-tooling drift.

This guide covers:

- The byte-stability contract.
- The compile-time allow-list (what is currently exempt and why).
- The `anodize check determinism` harness CLI.
- `--allow-nondeterministic <name>=<reason>`, the operator escape, and its
  three audit surfaces.
- Snapshot-mode `SOURCE_DATE_EPOCH` resolution.
- A worked example.

See also the companion [Reproducible Builds](./reproducible-builds.md) guide
for the user-facing config knobs that opt individual stages into byte-stable
output. This page documents the cross-pipeline contract and the verification
harness that audits it.

## The contract

Every artifact emitted by an anodize stage MUST be byte-stable across
rebuilds of the same commit at the same anodize version. Exceptions live on
a documented allow-list. Allow-listed artifacts carry an opt-out reason that
consumers can audit.

The mechanism is `SOURCE_DATE_EPOCH` (SDE). The pipeline computes the value
once at start-up, defaulting to the release commit's timestamp, and exports
it into every subprocess. Stages that emit timestamps consume SDE directly
(native CycloneDX SBOM, `tar`/`zip` writers, cosign 2.0+ signatures) or
through a tool flag (`gpg --faked-system-time`, BuildKit reproducible
flags).

Every per-publisher receipt (`PublishEvidence`) carries a
`nondeterministic: Option<String>` field. `None` means byte-stable;
`Some(reason)` means allow-listed (compile-time or runtime).

## Compile-time allow-list

Some artifacts are exempt out of the box because the relevant tooling does
not yet honor SDE or because byte-stability has been deferred to a
follow-up.

The allow-list is seeded in
[`crates/core/src/determinism.rs::seed_from_commit`](https://github.com/tj-smith47/anodizer/blob/master/crates/core/src/determinism.rs)
and contains exactly six glob patterns at HEAD:

| Artifact pattern | Reason | Mitigation |
|---|---|---|
| `*.crate` | `cargo package` is non-deterministic by default | consumers verify via the crates.io index, not anodizer-published checksums |
| `*.rpm` | `rpmbuild` reproducibility deferred to determinism-installers follow-up | consumers verify post-tool signatures (signtool, productsign, codesign) |
| `*.msi` | wix/candle/light reproducibility deferred to determinism-installers follow-up | consumers verify post-tool signatures |
| `*.dmg` | `hdiutil` reproducibility deferred to determinism-installers follow-up | consumers verify post-tool signatures |
| `*.pkg` | `pkgbuild` reproducibility deferred to determinism-installers follow-up | consumers verify post-tool signatures |
| `*.deb` | `dpkg-deb` reproducibility varies by version, follow-up | cloudsmith publisher byte-mismatch detection already loud-fails on retry |

Notably absent from the table (intentional, post-M10 cleanup): Docker
image manifest descriptors, Docker image blobs, NSIS-emitted `.exe`
installers, Apple notarization receipts, and external public-TSA
signatures. These artifacts mutate inside side-effect stages
(`crate::determinism_runner::SIDE_EFFECT_STAGES`) and are reproducible
by virtue of being skipped during determinism rebuilds; the harness
never diffs them. NSIS `.exe` files only appear on Windows/Wine runs,
where operators can use the runtime `--allow-nondeterministic
<name>=<reason>` flag rather than baking in a dead compile-time
sentinel. The Docker stage's only `dist/` output is a `.digest` text
file (content-addressable sha256), which is byte-stable without
allow-listing.

## `anodize check determinism`

The verification harness is a leaf of `anodize check`:

```
anodize check config [--workspace=<path>]
anodize check determinism \
  --runs=<N> \
  --stages=<subset> \
  --targets=<csv> \
  --report=<path> \
  --preserve-dist=<path> \
  [--snapshot | --no-snapshot]
```

| Flag | Default | Description |
|---|---|---|
| `--runs=<N>` | `2` | Number of from-clean rebuilds to diff against each other. |
| `--stages=<subset>` | full set | Restrict to a stage subset (`build,archive,sbom,sign,checksum`). |
| `--targets=<csv>` | (all) | Restrict the harness to a comma-separated subset of configured target triples (forwarded to the child `anodize release` subprocess). Used by the sharded release matrix so each runner only validates targets it can natively build. |
| `--report=<path>` | `dist/run-<id>/determinism.json` | JSON report destination. |
| `--preserve-dist=<path>` | off | On green, copy run-0's `<worktree>/dist/**` to `<path>` and emit `<path>/context.json`. The release workflow's `release --publish-only` step consumes this directly — eliminating a separate recompile job. See [Preserved raw binaries layout](#preserved-raw-binaries-layout) for how `binary_signs:` source binaries are mirrored alongside dist. |
| `--snapshot` / `--no-snapshot` | auto | Force snapshot mode on or off for the child release subprocess. Default: auto — `--no-snapshot` when HEAD is at a tag (`git describe --tags --exact-match HEAD` succeeds), `--snapshot` otherwise. Mutually exclusive. |

Scope: build-side only. The harness runs the pipeline up to and including
`checksum`. It never invokes `release`, `publish`, `blob`, `snapcraft-publish`,
or `announce`. Doubling `--runs=N` is safe in any environment because no
external side effects fire.

Each run executes inside a freshly-constructed environment:

| Variable | Behavior |
|---|---|
| `CARGO_HOME` | Per-run tmpdir under the worktree (`.det-tmp/cargo/`); never shared across runs. |
| `CARGO_TARGET_DIR` | Per-run tmpdir under the worktree (`.det-tmp/target/`); never shared. |
| `RUSTUP_HOME` | Inherited from host when set; otherwise synthesized as `<host HOME>/.rustup` so rustup can dispatch a toolchain in the sealed env. |
| `SOURCE_DATE_EPOCH` | Computed once per harness invocation; exported into every run. |
| `TMPDIR`, `HOME` | Per-run tmpdirs under the worktree to neutralize dot-file influence on build scripts. |
| `PATH` | Inherited from host verbatim. Two harness runs from the same host process see identical PATH, so determinism is preserved without per-platform allow-list maintenance. |
| `RUSTFLAGS` | `--remap-path-prefix=<worktree>=/anodize` is appended (plus `<cargo_home>=/cargo` and `<cargo_target>=/target`) so absolute paths don't leak into the binary. Host-supplied RUSTFLAGS are preserved. |
| `CARGO_TARGET_<MSVC_TRIPLE>_RUSTFLAGS` | Injects MSVC determinism flags (`/Brepro`, `/OPT:NOICF`, `/INCREMENTAL:NO`, `/DEBUG:NONE`, `-C strip=symbols`, `-C codegen-units=1`) for the two `*-pc-windows-msvc` targets. On Windows, also appended to global `RUSTFLAGS` so the host build (e.g. a `before:` hook's `cargo run`) is reproducible too. |
| Linux / macOS env | Everything else stripped except an identity-only allow-list: `CI`, `RUSTUP_HOME`, plus the named identity vars `GITHUB_REPOSITORY`, `GITHUB_SHA`, `GITHUB_REF`, `GITHUB_REF_NAME`, `GITHUB_RUN_ID`, `GITHUB_RUN_NUMBER`, `GITHUB_WORKFLOW`, `GITHUB_ACTOR`, `RUNNER_OS`, `RUNNER_ARCH`, `RUNNER_NAME`. |
| Windows env | Inverse: inherits the full host env (MSVC's `VC*` / `VS*` / `INCLUDE` / `LIB` / `LIBPATH` / `WindowsSdk*` / `UCRT*` plus `PROGRAMFILES*` / `WINDIR` / `SystemRoot` / `USERPROFILE` / `APPDATA` / `LOCALAPPDATA` / `TEMP` / `TMP` / `PATHEXT`) then drops a credential deny-list (`GITHUB_TOKEN`, `CARGO_REGISTRY_TOKEN`, `AWS_*`, `COSIGN_*`, `GPG_*`, ...), a suffix sweep (`_TOKEN` / `_KEY` / `_SECRET` / `_PASSWORD` / `_PASSPHRASE` / `_CREDENTIALS`), `ACTIONS_*`, and any `GITHUB_*` / `RUNNER_*` not on the identity allow-list (e.g. `RUNNER_TEMP`, `GITHUB_WORKSPACE` — host workflow state, not identity). |

The workspace under test is obtained via `git worktree add` rooted at the
release commit, so gitignored files (notably `target/`, `dist/`,
`node_modules/`) cannot leak between runs.

For each emitted artifact, the harness computes SHA256 and diffs across
runs. Artifacts whose `PublishEvidence.nondeterministic = Some(_)` are
excluded from the diff. The harness exits non-zero on any drift and prints
a report enumerating each offending artifact with a `differing_bytes_summary`
heuristic that names the first offset where the head sample diverges
(e.g. `"first diff at offset 0x108 (run0=0xd6, run1=0x51)"`).

When drift is detected, the harness also dumps the full drifted binaries
from both runs to `dist/run-<id>/drift-bins/run-<N>/<artifact>`. In CI,
the release workflow uploads this tree alongside the JSON report so
operators can `gh run download` the actual bytes and run external diff
tools (`cmp -l`, `xxd`, etc.) without re-running the harness.

The Taskfile target `task check:determinism` invokes the harness with
default args.

## The operator escape

`--allow-nondeterministic <name>=<reason>` is a per-release escape for
emergency cases where a third-party tool's reproducibility breaks
unexpectedly. The flag is **repeatable**, not comma-separated:

```bash
anodize release \
  --allow-nondeterministic foo.rpm=tool-bug-1234 \
  --allow-nondeterministic bar.msi=signing-cert-rotation
```

Semantics:

- Each invocation appends to the per-run allow-list.
- Reasons may contain any characters except newline; names must match an
  emitted artifact (mismatched names error out before any publish).
- Pairs are mirrored into three audit surfaces (see below).
- **Precedence on collision**: when a runtime opt-out names an artifact that
  also has a compile-time allow-list entry, the compile-time reason wins on
  the `PublishEvidence.nondeterministic` field and both entries appear in
  the report. The operator flag adds entries; it never overrides existing
  ones.
- **`--strict` interaction**: under `--strict`, `--allow-nondeterministic`
  is rejected at CLI parse time with a clear error pointing to this guide.
  Production releases that need an exemption must drop `--strict`, which
  already surfaces the elevated risk.

## Three audit surfaces

Every allow-listed artifact (compile-time or runtime) shows up in three
places so a consumer cannot miss it:

1. **Run summary JSON** (`--summary-json=<path>`). The `determinism_allowlist`
   key contains `compile_time` and `runtime` arrays.

2. **Determinism report** (`dist/run-<id>/determinism.json`). The `allowlist`
   key contains the same two arrays plus the per-artifact decision under
   `artifacts[]`.

3. **GitHub release body**. A `Non-deterministic exemptions:` section is
   appended above the SHA256SUMS block so consumers see opt-outs without
   parsing JSON. Example:

   ```
   Non-deterministic exemptions:
   - foo.rpm: tool-bug-1234
   - anodizer-0.2.1.crate: cargo package non-determinism, tracked in determinism-followups

   SHA256SUMS:
   ...
   ```

## The determinism report

The report lives at `dist/run-<id>/determinism.json` (single dist namespace
shared with the failure-handling run report). Shape:

```json
{
  "schema_version": 1,
  "anodize_version": "0.2.1",
  "commit": "abc123...",
  "commit_timestamp": 1715000000,
  "runs": 2,
  "stages_under_test": ["build", "archive", "sbom", "sign", "checksum"],
  "allowlist": {
    "compile_time": [
      { "artifact": "anodizer-0.2.1.crate", "reason": "cargo package non-determinism, tracked in determinism-followups" }
    ],
    "runtime": [
      { "artifact": "foo.rpm", "reason": "tool-bug-1234" }
    ]
  },
  "artifacts": [
    {
      "name": "anodizer_0.2.1_linux_amd64.tar.gz",
      "path": "dist/anodizer_0.2.1_linux_amd64.tar.gz",
      "size_bytes": 5242880,
      "stage": "archive",
      "deterministic": true,
      "hash": "sha256:..."
    },
    {
      "name": "anodizer-0.2.1.crate",
      "path": "dist/anodizer-0.2.1.crate",
      "size_bytes": 1048576,
      "stage": "cargo-package",
      "deterministic": false,
      "nondeterministic_reason": "cargo package non-determinism, tracked in determinism-followups",
      "hashes": ["sha256:...", "sha256:..."]
    }
  ],
  "drift": [],
  "drift_count": 0
}
```

`schema_version: 1` so downstream CI parsers fail loudly on shape change.
Unknown fields are rejected on the producer side; consumers may ignore
unknown fields per JSON convention.

## Snapshot-mode SDE resolution

`anodize release --snapshot` must produce byte-identical artifacts across
runs of the same commit at the same anodize version. SDE source for snapshot
mode (first match wins):

1. `ANODIZE_SOURCE_DATE_EPOCH` env var, if set.
2. `HEAD` commit timestamp, if the working tree is clean. "Clean" is
   defined as `git status --porcelain --untracked-files=normal --ignore-submodules=none`
   producing empty output.
3. Hash of `git status --porcelain=v2 -z` output (truncated to a 32-bit
   value) added to the `HEAD` commit timestamp, when the tree is dirty.
   Deterministic per tree state; does not require a writable index, so
   read-only worktrees produce the same value.

This is what makes the harness useful pre-release: an operator can run
`anodize check determinism --snapshot` against a dirty tree and catch drift
before tagging.

## Worked example

Run the harness with two from-clean rebuilds:

```bash
anodize check determinism --runs=2
```

Output (abbreviated):

```
anodize check determinism: runs=2 stages=build,archive,sbom,sign,checksum
  run 1: 18.4s  (4 artifacts emitted)
  run 2: 17.9s  (4 artifacts emitted)
  diff:  0 artifacts drifted
  allow-list:
    compile_time: anodizer-0.2.1.crate (cargo package non-determinism, tracked in determinism-followups)
    runtime:      (none)
  report: dist/run-20260514T142301Z/determinism.json
PASS
```

Inspect the report:

```bash
cat dist/run-20260514T142301Z/determinism.json | jq '.drift_count, .artifacts[].deterministic'
```

A non-zero `drift_count` (or any `deterministic: false` without a matching
`nondeterministic_reason`) is a release blocker. Run the harness with
`--stages=<offending-stage>` to bisect, then fix the underlying source of
drift (timestamp embed, file-order non-determinism, embedded GUID).

### `--publish-only` auto-enables `resume_release`

When `anodize release --publish-only` runs, `resume_release` is automatically
set to `true`. This lets the publish-only job proceed even when the release
stage previously uploaded some assets (the prior determinism-harness run on the
same tag left a partial release on disk). Without this implicit flag, the release
stage would refuse to continue after detecting leftover assets and bail with a
"prior report.json exists" error.

Operators do not need to pass `--resume-release` manually for the standard
determinism → preserve-dist → `--publish-only` pattern.

### Preserved raw binaries layout

`--preserve-dist=<path>` copies `<worktree>/dist/**` so the downstream
`release --publish-only` job has the artifact tree it needs. **But** the
per-stage `binary_signs:` block signs the raw cargo build outputs that live
*outside* `dist/` — at `<worktree>/.det-tmp/target/<triple>/release/<basename>`
under the harness's `CARGO_TARGET_DIR` override. Those binaries are *not*
in `dist/**`; without explicit preservation, the publish-only loader would
either skip them or crash on missing files.

Layout (as written by `preserve_raw_binaries` in
[`crates/cli/src/determinism_harness/preserve.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/determinism_harness/preserve.rs)):

```
<preserve-dist>/
├── context.json
├── artifacts.json                  # rewritten: Binary entries now point at _preserved-bin/...
├── <archive>.tar.gz                # normal dist/** contents
├── ...
└── _preserved-bin/                 # raw binaries mirrored out of the worktree
    ├── x86_64-unknown-linux-gnu/
    │   └── anodizer
    ├── aarch64-unknown-linux-gnu/
    │   └── anodizer
    └── x86_64-pc-windows-msvc/
        └── anodizer.exe
```

- **Path constant:** `PRESERVED_BIN_SUBDIR = "_preserved-bin"` —
  single source of truth shared by the manifest rewrite, the disk
  copy, and the publish-only loader's `dist/`-prefix re-anchor.
- **Underscore prefix (not dot-hidden):** `_preserved-bin/` is *visible*
  to `actions/upload-artifact@v4` without setting
  `include-hidden-files: true`. A dotfile would silently drop out of
  the uploaded artifact and `binary_signs:` would fail with missing
  inputs on the publish-only runner.
- **Why this exists:** publish-only loads preserved dist on a fresh runner
  with no `target/` tree — the raw binary bytes that
  `binary_signs: { artifacts: binary, cmd: cosign }` operates on must
  travel with the dist tree. The prior session's
  `suppress_binary_signs` workaround (which silently skipped binary
  signing during publish-only) was deleted in commit `596e1a3` in favour
  of this preservation path.
- **What's preserved:** every `Binary`, `UploadableBinary`, `Library`,
  `Header`, `CArchive`, `CShared`, and `Wasm` artifact from
  `artifacts.json`. `UniversalBinary` is deliberately excluded —
  `stage-build`'s universal step writes lipo'd output into `dist/`
  already, so it's caught by `preserve_dist_tree` directly.

### Makeself artifact ordering

The makeself stage groups artifacts by platform before registering them with
the artifact store. The grouping uses `BTreeMap` (sorted, deterministic) rather
than `HashMap` (randomized per process). This ensures the per-platform iteration
order is identical across determinism runs and does not introduce drift into
`dist/artifacts.json`. The same fix applies to the snapcraft stage.

In CI, the determinism check runs as a fan-out matrix that doubles as
the build step. Each shard validates one platform's targets and
uploads its byte-stable `dist/` under `dist-<shard>`; the downstream
`release:` job downloads every shard's preserved dist and runs
`anodize release --publish-only` against the merged tree. The release
proceeds only when every shard passes. Anodizer's own release workflow
uses this shape:

```yaml
jobs:
  determinism-check:
    name: Determinism Harness (${{ matrix.shard }})
    strategy:
      fail-fast: false
      matrix:
        include:
          - { os: ubuntu-latest,  shard: ubuntu-latest,    targets: '' }
          - { os: macos-latest,   shard: macos-latest,     targets: '' }
          - { os: windows-latest, shard: windows-x86_64,   targets: 'x86_64-pc-windows-msvc' }
          - { os: windows-latest, shard: windows-aarch64,  targets: 'aarch64-pc-windows-msvc' }
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v6
      - uses: tj-smith47/anodizer-action@v1
        with:
          determinism: true
          determinism-targets: ${{ matrix.targets }}
          preserve-dist: 'true'
          shard-label: ${{ matrix.shard }}
      - name: Upload dist artifacts
        if: success()
        uses: actions/upload-artifact@v4
        with:
          name: dist-${{ matrix.shard }}
          path: preserved-dist/
          if-no-files-found: error
      - name: Upload determinism report
        if: always()
        uses: actions/upload-artifact@v4
        with:
          name: determinism-${{ matrix.shard }}
          path: |
            dist/run-*/determinism.json
            dist/run-*/drift-bins/**

  release:
    needs: determinism-check
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v6
        with: { fetch-depth: 0 }
      - uses: actions/download-artifact@v4
        with:
          path: dist/
          pattern: dist-*
          merge-multiple: true
      - uses: tj-smith47/anodizer-action@v1
        with:
          args: release --publish-only
        env:
          GITHUB_TOKEN: ${{ secrets.GH_PAT }}
```

Windows is split per-target because it's the slowest platform; pinning
each shard to a single MSVC triple halves wall-clock on the critical
path. Linux and macOS run their full target list in a single shard
because both complete inside the Windows envelope.

`determinism-targets: ''` lets the action pick the targets matching
`RUNNER_OS` from `.anodizer.yaml`'s configured target list. An explicit
value overrides that selection for the shard.

`preserve-dist: 'true'` reuses the harness's run-0 dist as the release's
build output, eliminating a recompile pass. `shard-label` is required
because `merge-multiple: true` would otherwise collide each shard's
`context.json` / `artifacts.json` on the consumer side.

### Multi-shard hash-verify tolerance

Each shard hash-verifies only the artifacts produced by its own targets.
Hashes from shard A are never compared against shard B's `dist/` — there is
no cross-shard hash comparison at all. The invariant the harness enforces is:

> Within a single shard, two runs of the same target list must produce
> byte-identical artifacts.

This is what makes 3-way (and 4-way, in anodizer's own case) matrix
sharding possible. A Linux runner can never natively build the
`*-pc-windows-msvc` triples; a macOS runner can't produce
`x86_64-unknown-linux-musl`. If the harness required all shards to
agree on every artifact's hash, sharding would be impossible — every
shard would either need a complete toolchain (defeating the wall-clock
win) or be forced into emulated cross-compilation (which is not
byte-stable across rebuild hosts).

The tolerance is intentionally one-directional: it relaxes the
*cross-shard* comparison while keeping the *per-shard* contract
strict. A single shard that produces drifting hashes between run-0
and run-1 of its own target subset still fails the harness exactly
as a single-shard run would. The downstream `release:` job that
merges every shard's `dist/` and runs `--publish-only` doesn't
re-verify either — it trusts that each shard already validated its
own outputs and treats the merged tree as authoritative.

PR builds run the same harness with a fast advisory subset
(`--stages=archive,sbom,sign,checksum`) on a single Linux shard via the
action's `determinism-stages` input.
