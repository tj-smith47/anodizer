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

| Artifact | Reason | Mitigation |
|---|---|---|
| cargo `.crate` | `cargo package` is non-deterministic by default | consumers verify via the crates.io index, not anodizer-published checksums |
| Docker image manifest descriptor | cosign attestation blobs embed timestamps | image digest is content-addressable; consumers verify the digest |
| Docker image | BuildKit reproducible-build flags integration deferred | image digest content-addressable; consumers verify the digest |
| RPM / MSI / NSIS / DMG / PKG installers | per-tool reproducibility conventions, follow-up work | consumers verify post-tool signatures (signtool, productsign, codesign) |
| DEB (nfpm-emitted) | `dpkg-deb` reproducibility varies by version, follow-up | cloudsmith publisher byte-mismatch detection already loud-fails on retry |
| External-service signatures (Apple notarization, public TSA) | server embeds a timestamp anodize cannot control | documented allow-list entry per publisher |

The canonical follow-up list lives at
[`.claude/specs/2026-05-14-determinism-followups.md`](https://github.com/tj-smith47/anodizer/blob/master/.claude/specs/2026-05-14-determinism-followups.md).
Each entry is its own future workstream.

## `anodize check determinism`

The verification harness is a leaf of `anodize check`:

```
anodize check config [--workspace=<path>]
anodize check determinism \
  --runs=<N> \
  --stages=<subset> \
  --report=<path> \
  --snapshot
```

| Flag | Default | Description |
|---|---|---|
| `--runs=<N>` | `2` | Number of from-clean rebuilds to diff against each other. |
| `--stages=<subset>` | full set | Restrict to a stage subset (`build,archive,sbom,sign,checksum`). |
| `--report=<path>` | `dist/run-<id>/determinism.json` | JSON report destination. |
| `--snapshot` | off | Seed SDE from snapshot rules (env > HEAD > dirty-tree hash) instead of the release commit. |

Scope: build-side only. The harness runs the pipeline up to and including
`checksum`. It never invokes `release`, `publish`, `blob`, `snapcraft-publish`,
or `announce`. Doubling `--runs=N` is safe in any environment because no
external side effects fire.

Each run executes inside a freshly-constructed environment:

| Variable | Behavior |
|---|---|
| `CARGO_HOME` | Per-run tmpdir, prepopulated from a vendor archive committed to the workspace. |
| `CARGO_TARGET_DIR` | Per-run tmpdir; never shared across runs. |
| `RUSTUP_HOME` | Inherited from host (toolchain pinned via `rust-toolchain.toml`). |
| `SOURCE_DATE_EPOCH` | Computed once per harness invocation; exported into every run. |
| `TMPDIR`, `HOME` | Per-run tmpdirs to neutralize dot-file influence on build scripts. |
| `PATH` | Trimmed to an explicit allow-list (`/usr/bin:/bin:<toolchain-bin>`) plus anodize's tool-detect paths. |
| Everything else | Stripped except an explicit allow-list (`CI`, `RUNNER_*`, `GITHUB_*`). |

The workspace under test is obtained via `git worktree add` rooted at the
release commit, so gitignored files (notably `target/`, `dist/`,
`node_modules/`) cannot leak between runs.

For each emitted artifact, the harness computes SHA256 and diffs across
runs. Artifacts whose `PublishEvidence.nondeterministic = Some(_)` are
excluded from the diff. The harness exits non-zero on any drift and prints a
report enumerating each offending artifact with structured drift context
(timestamp fields, tar entry ordering, embedded GUIDs) where the heuristic
can locate the differing bytes, raw hex diff otherwise.

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

In CI, the determinism check runs before the release job:

```yaml
jobs:
  determinism-check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with: { fetch-depth: 0 }
      - run: cargo build --release -p anodizer-cli
      - run: ./target/release/anodizer check determinism --runs=2

  release:
    needs: determinism-check
    runs-on: ubuntu-latest
    steps:
      - # existing release steps
```

The release job is blocked when the determinism check fails. PR builds run
the same harness with a fast subset (`--stages=archive,sbom,sign,checksum`)
as an advisory check.
