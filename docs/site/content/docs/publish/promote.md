+++
title = "Promote"
description = "Move an already-published release from a pre-release track to stable, without rebuilding"
weight = 14
template = "docs.html"
+++

`anodizer promote` moves an artifact you already published from a pre-release
track to a stable track — **without rebuilding**. It re-points the registry's
own pointer: a snapcraft channel, an npm dist-tag, an OCI floating tag, or a
GitHub release's `prerelease` flag. The bytes never change; only which track
resolves to them does.

This is the "release candidate" workflow: publish `1.4.0` to a candidate track,
let it soak, then promote the exact reviewed revision to stable once you trust
it.

```bash
# Publish a candidate (in your release config / pipeline), then later:
$ anodizer promote --to stable
   • promoted snapcraft anodizer rev 42 candidate→stable
   • re-tagged npm anodizer@1.4.0 next→latest
   • re-pointed docker ghcr.io/acme/app:edge → ghcr.io/acme/app:latest
   • flipped github release v1.4.0 prerelease→stable
```

## No config block — and why

Promotion is **CLI-driven only**. There is deliberately no `promote:` config
field. A static `promote: {from: candidate, to: stable}` would run on *every*
release and auto-promote the revision you just uploaded — defeating the entire
point of a candidate gate. Promotion is a decision a human (or a gated CI job)
makes *after* a soak, so it lives on the command line where that intent is
explicit.

anodizer reads your existing publisher config (`snapcrafts:`, `npms:`,
`dockers_v2:`, `release.github`) only to learn each publisher's native track
vocabulary and locate its repos — never to trigger a promotion.

## Track vocabulary

Pass a **canonical** track (`stable`, `prerelease`, `candidate`, `beta`,
`edge`) and each publisher maps it to its own native track. A publisher-native
name (e.g. an npm dist-tag you invented) passes through verbatim.

| Canonical `--to` | snapcraft channel | npm dist-tag | docker tag | GitHub release |
|---|---|---|---|---|
| `stable` | `stable` | `latest` | `latest` | clear `prerelease` + make latest |
| `candidate` | `candidate` | your pre-tag¹ | `edge` | set `prerelease` |
| `beta` | `beta` | your pre-tag¹ | `edge` | set `prerelease` |
| `edge` | `edge` | your pre-tag¹ | `edge` | set `prerelease` |
| `prerelease` | `candidate` | your pre-tag¹ | `edge` | set `prerelease` |

¹ npm's pre-stable dist-tag is your `npms[].tag` when it names a non-`latest`
tag, otherwise `next`.

`--from` (default `prerelease`) is the source track. It is informational for the
publishers that locate the artifact by version or by "newest pre-release"; it
selects the source floating tag for docker.

## Selecting which artifact to promote

| Selector | Flag | Behavior |
|---|---|---|
| Newest | *(default)* | The newest artifact currently on the `--from` track. |
| Explicit version | `--version 1.4.0` | Promote exactly this version/tag. |
| Prior run | `--from-run <id>` | Promote what a recorded run published (reads `dist/run-<id>/report.json`). `--from-run` is the most precise: it moves exactly the revisions that run uploaded, per its recorded evidence. |

```bash
$ anodizer promote --to stable --version 1.4.0
$ anodizer promote --to stable --from-run 20260712-abc123
```

## Choosing publishers

By default every configured, promotion-capable publisher runs. Narrow with
`--publishers`:

```bash
$ anodizer promote --to stable --publishers docker,github
```

Naming a publisher that does not support promotion is a hard error:

```bash
$ anodizer promote --to stable --publishers cargo
error: publisher 'cargo' does not support promotion (promotable: snapcraft, npm, docker, github)
```

Promotion-capable publishers: **snapcraft**, **npm**, **docker**, **github**.
(cargo, PyPI, and the index publishers publish immutable versions with no
mutable track pointer to move.)

## What each publisher does

| Publisher | Mechanism | Rebuild? |
|---|---|---|
| snapcraft | `snapcraft release <name> <rev> <channel>` | no |
| npm | `npm dist-tag add <pkg>@<version> <tag>` for every platform package | no |
| docker | `docker buildx imagetools create --tag <repo>:<to> <repo>:<from>` (registry-side manifest copy) | no |
| github | `PATCH /repos/{owner}/{repo}/releases/{id}` flipping `prerelease` | no |

npm re-tags the **whole package family** — the metapackage and every
per-platform package — so a promoted release is consistent across every install
target. docker and github operate on every configured image repo / release repo,
deduplicated so a lockstep workspace sharing one tag flips it once.

## Credentials

Promotion needs the same credentials as the original publish:

- **snapcraft** — a logged-in `snapcraft` (Snap Store credentials).
- **npm** — `NPM_TOKEN` (OIDC publish credentials cannot move a dist-tag).
- **docker** — `docker buildx` authenticated to the registry.
- **github** — `ANODIZER_GITHUB_TOKEN` / `GITHUB_TOKEN` / `GH_TOKEN`, or `--token`.

A live promotion preflights every selected publisher's tool and credentials and
**fails fast** before mutating anything — so a missing token stops the run
before the first registry is touched, never halfway through.

## Dry run

`--dry-run` resolves the full plan and prints exactly what would happen, running
no external command and requiring no credential:

```bash
$ anodizer promote --to stable --dry-run
   • (dry-run) would promote snapcraft newest candidate→stable
   • (dry-run) would promote npm newest next→latest
   • (dry-run) would re-point docker ghcr.io/acme/app:edge → ghcr.io/acme/app:latest
   • (dry-run) would flip github release newest on acme/app (prerelease→stable)
```

Run the dry-run first whenever you are unsure which artifact the selector
resolves to.
