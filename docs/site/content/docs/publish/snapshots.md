+++
title = "Snapshots"
description = "Build locally without publishing"
weight = 9
template = "docs.html"
+++

Snapshot mode runs the full build and archive pipeline but skips all publishing stages.

## Classification

Not applicable — this is a workflow page, not a publisher. Snapshot mode disables every publisher in the pipeline (including Submitters); the only outputs are local artifacts under `dist/`.

## Minimal config

```bash
anodizer release --snapshot
```

No YAML changes required for the default behavior.

## Full config reference

```yaml
snapshot:
  version_template: "{{ .Version }}-SNAPSHOT-{{ .ShortCommit }}"  # optional; version suffix (alias: name_template)
```

## Authentication

Not applicable — snapshot mode never contacts external services. No tokens are read or required.

## Common gotchas

- The default template appends `-SNAPSHOT` to the version; override via `snapshot.version_template` (or its deprecated alias `name_template`).
- `--auto-snapshot` engages snapshot mode whenever the git repo has uncommitted changes — useful for safety in CI.
- Required publishers are silently skipped; snapshots never publish regardless of the `required` flag.

## Auto-snapshot

Automatically enable snapshot mode when the git repo has uncommitted changes:

```bash
anodizer release --auto-snapshot
```

## Emission validation {#emission-validate}

Some publishers don't push an asset — they *emit a reference to one*: the
binstall `[package.metadata.binstall]` block names a download URL, the Nix
derivation maps each `packages.<system>` to an asset, version-sync writes a
crate version. A real release mutates source files or pushes to a remote; a
snapshot/dry-run skips those side effects. Historically that meant a **broken**
emission — a binstall URL pointing at an asset the release never produces, a
nix system mapped to a missing asset — passed every local check and only blew
up later at `cargo binstall` / `nix build` time on a consumer's machine.

Snapshot/dry-run closes that blindspot. anodizer renders the would-be emission
in-memory (never mutating source, never cloning, never pushing) and
cross-checks it against the asset set the run actually produced. A mismatch
fails the snapshot loud, naming the crate, the emission, and what's wrong:

```bash
$ anodizer release --snapshot --host-targets
error: binstall emission for crate 'myapp' references an asset the release does
       not produce: myapp-1.2.3-linux-amd64.tar.gz (produced: myapp-1.2.3-x86_64-unknown-linux-gnu.tar.gz)
```

This runs per in-scope crate with that crate's own version/name/tag scope, so
it's correct in single-crate, workspace-lockstep, per-crate, and `--all` modes.
It is the local gate that catches the "release succeeds but is silently wrong"
class before you push.

## Two local gates: slim `snapshot` vs full `prepush`

anodizer's own `Taskfile.yml` wires two snapshot gates with different
cost/coverage trade-offs. A consumer project can mirror the same shape:

| Task | What it runs | When |
|---|---|---|
| `task snapshot` | dry-run pipeline, `--single-target`, no real build — fast | wired into the commit hook (via `task lint` → `task commit`) |
| `task prepush` | **real** build of every host-buildable target (`--host-targets`) + archive/sign/checksum + emission validation | before pushing |

```bash
# Slim, fast — compiles nothing, single target. Runs on every commit.
$ anodizer release --snapshot --single-target --clean --dry-run

# Full — real host-scoped build of everything this host can compile, plus the
# emission-validate cross-check above. Slower; run before pushing.
$ anodizer release --snapshot --clean --host-targets
```

The slim gate keeps commits fast; the full gate is the one that actually
compiles artifacts and validates binstall/nix/version-sync emissions against
them. See [`--host-targets`](@/docs/builds/single-target.md#host-targets) for
how the host-scoped target set is computed (apple targets are skipped off a
non-macOS host, logged loudly).
