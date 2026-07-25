+++
title = "Release workflow (prepare / publish / announce)"
description = "Every release invocation, the stages it runs, and which one to reach for"
weight = 11
template = "docs.html"
+++

Anodizer's release pipeline is one ordered list of stages. Most operators run it end-to-end via `anodizer release`, but a handful of dedicated entry points let you split, resume, or re-fire individual phases without rebuilding from scratch. This page enumerates every supported invocation and exactly which stages it runs.

## State machine

| Invocation | Stages run | Stages skipped | Use case |
|---|---|---|---|
| `anodizer release` | all | none | normal release |
| `anodizer release --snapshot` | local stages | blob, publish, snapcraft-publish, announce | local dry-run (no upstream side effects) |
| `anodizer release --prepare` (alias `--prepare-only`) | build, archive, nfpm, sbom, checksum, sign | release, docker, docker-sign, blob, publish, snapcraft-publish, announce, verify-release | split-merge flow: prepare artifacts locally before manual review |
| `anodizer release --publish-only` | sign, release, blob, publish, snapcraft-publish, announce | build, archive, nfpm, sbom, checksum | resume from prepared `dist/` after manual review or after the Determinism Harness preserved `dist/` |
| `anodizer release --announce-only` | announce, after-hooks | every other stage | re-fire announcers after a transient announce failure (Slack 502, Discord 5xx) |
| `anodizer publish` | release, blob, publish, snapcraft-publish | every other stage | publish-only subcommand; overlaps with `release --publish-only` (see below) |
| `anodizer publish --merge` | shard-merge → release, blob, publish, snapcraft-publish | every other stage | split-merge multi-host flow (mirrors GR Pro `goreleaser publish --merge`) |
| `anodizer announce` | announce | every other stage | announce-only subcommand |
| `anodizer announce --merge` | shard-merge → announce | every other stage | split-merge multi-host flow (mirrors GR Pro `goreleaser announce --merge`) |
| `anodizer continue` | release, blob, publish, snapcraft-publish, announce, after-hooks | build, archive, nfpm, sbom, checksum, sign | single-host stage-resume (paused release, transient publish failure) |
| `anodizer continue --merge` | shard-merge → sign, checksum, sbom, release, blob, publish, snapcraft-publish, announce | build, archive, nfpm | multi-host split-merge resume (mirrors GR Pro `goreleaser continue --merge`) |

`anodizer release --rollback-only --from-run=<id>` is an additional escape hatch for the post-failure recovery flow; see [recovery flags](@/docs/advanced/recovery-flags.md) for the surrounding context.

For composition with the `--skip=` flag (used to drop individual stages from any invocation above), see the inline help on `anodizer release --skip --help`.

## `release --prepare` vs `release --publish-only`

These are the two halves of the split-merge flow.

```bash
# Phase 1: prepare dist on the build host
anodizer release --prepare

# (manual review of dist/ — diff archives, verify checksums, ...)

# Phase 2: publish from the same host
anodizer release --publish-only
```

`--prepare` runs every artifact-producing stage (build / archive / nfpm / sbom / checksum / sign) and leaves them in `dist/`. `--publish-only` consumes that tree and runs the upload chain.

The `--prepare-only` alias exists for GR-imported scripts; it is a literal alias for `--prepare`.

## `release --announce-only` vs `anodizer announce`

Both re-fire announcers without re-publishing.

```bash
# Flag form: re-fire against the configured dist/ + the prior run's report.json
anodizer release --announce-only

# Subcommand form: same, but accepts --dist to point at a non-default tree
anodizer announce --dist /path/to/preserved-dist
```

`release --announce-only` derives the run id from the current git tag / short commit (matching the writer that produced `<dist>/run-<id>/report.json`); `anodizer announce` does not require the report file and announces fresh from the dist's `artifacts.json`.

Both honor the nightly short-circuit — announcers never fire on a nightly tag, matching GoReleaser's `customization/publish/nightlies.md` rule.

## `publish` vs `continue`

Both consume a populated `dist/` and run the release / blob / publish chain. They differ in framing and post-hooks:

| Aspect | `anodizer publish` | `anodizer continue` |
|---|---|---|
| Stages | release, blob, publish, snapcraft-publish | release, blob, publish, snapcraft-publish, announce, after-hooks |
| Framing | "run the publish chain" | "resume a stalled release" |
| GR Pro analog | `goreleaser publish` | `goreleaser continue` |
| `--merge` mode | shard-merge then publish | shard-merge then full post-build pipeline |
| Recommended for | publishing a one-off dist tree | resuming a paused (`--prepare`) or stalled release |

Neither is being deprecated. Prefer `continue` for the resume-after-failure use case; reach for `publish` when you explicitly want the unframed publish chain without the announce / after-hook fan-out.

## Idempotency of `--publish-only` retries

`release --publish-only` is safe to re-run against a `dist/` that already
has a `<dist>/run-<id>/report.json`. Publishers converge: each one
reconciles against its own upstream before dispatching and skips itself
when this exact version is already landed there.

```bash
$ anodizer release --publish-only
• publish: cargo: skipped — already published (crates.io has 0.2.1)
• publish: homebrew: skipped — already published (open PR homebrew-tap#41)
• publish: gemfury: publishing anodizer 0.2.1
```

Reconciliation fails toward publishing — a publisher skips only on a full
positive match of package name **and** version upstream, so an unreachable
registry or an ambiguous response still attempts the publish rather than
assuming it succeeded.

Two things this does NOT replace:

1. **Rollback.** `anodizer release --rollback-only --from-run=<id>` still
   exists to *revert* a partial publish. Re-running converges forward; it
   never undoes anything.
2. **Content drift.** If a version is already published upstream with
   different content, that publisher records a failure telling you to bump
   the version. Immutable releases cannot be overwritten by re-running.

The contract for `report.json`: it exists for replay (the `--rollback-only`
flow) and for human triage. Skip decisions come from the live upstream
state, not from the report.

## See also

- [Split / merge (distributed builds)](@/docs/advanced/split-merge.md) — for the `--merge` half of the split-merge flow.
- [Determinism Harness](@/docs/advanced/determinism.md) — for the `--preserve-dist` source that `release --publish-only` typically consumes.
- [Release resilience](@/docs/advanced/release-resilience.md) — for the `--rollback-only` flow referenced above.
- [Recovery flags](@/docs/advanced/recovery-flags.md) — for per-publisher overrides that change the recovery semantics.
