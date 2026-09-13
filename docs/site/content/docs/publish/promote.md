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

```console
# Publish a candidate (in your release config / pipeline), then later:
$ anodizer promote --to stable --dry-run
   • (dry-run) would promote snapcraft app newest candidate→stable
   • (dry-run) would promote npm newest next→latest
   • (dry-run) would promote docker ghcr.io/acme/app:edge → ghcr.io/acme/app:latest
   • (dry-run) would flip github release newest on acme/app (prerelease→stable)
   • snapcraft: candidate→stable (dry-run)
   • npm: next→latest (dry-run)
   • docker: 1 image(s) edge→latest (dry-run)
   • github: 1 release(s) prerelease→stable (dry-run)
```

Every run prints two registers: one line per artifact the publisher acted on,
then one folded summary line per publisher. Drop `--dry-run` to apply it.

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

### Snapcraft channel grammar

Both `--from` and `--to` are checked against the Snap Store's channel grammar
before anything is spawned — including under `--dry-run`, which is exactly
where a typo should surface. The form is `[<track>/]<risk>[/<branch>]`, with
`<risk>` one of `stable`, `candidate`, `beta`, `edge`:

| Accepted | Rejected |
|---|---|
| `stable`, `candidate`, `beta`, `edge` | `lastest`, `chanidate` — no risk word |
| `latest/candidate`, `2.x/stable` | `totally/bogus` — no risk word |
| `stable/hotfix-1` | `a/b/stable` — two tracks |
| `latest/stable/hotfix-1` | `stable/beta` — two risk words |

```console
$ anodizer promote --to lastest --publishers snapcraft --dry-run
   • snapcraft: candidate→lastest (failed: promote --to: invalid snapcraft channel 'lastest': expected [<track>/]<risk>[/<branch>] with <risk> one of stable, candidate, beta, edge (e.g. stable, latest/candidate, 2.x/stable, latest/stable/hotfix-1))
       Error 1 publisher(s) failed to promote: snapcraft
```

The same check runs on every rendered `snapcrafts[].channel_templates` entry
during a publish, so a template that only resolves to a bad channel at upload
time is caught before `snapcraft upload --release=` sees it.

## Promoting a snap from candidate to stable

The classic soak workflow: publish to `candidate`, test the real snap, then move
that exact revision to `stable`.

```console
$ anodizer promote --to stable --from candidate --publishers snapcraft --dry-run
   • (dry-run) would promote snapcraft myapp newest candidate→stable
   • snapcraft: candidate→stable (dry-run)
```

Drop `--dry-run` to apply it. A live run resolves the concrete revision per
architecture (`snapcraft list-revisions`), releases each one
(`snapcraft release <name> <rev> stable`), and prints a `promoted snap <name>
revision <rev> candidate→stable` result line per revision followed by the
folded per-publisher summary.

## Selecting which artifact to promote

| Selector | Flag | Behavior |
|---|---|---|
| Newest | *(default)* | The newest artifact currently on the `--from` track. |
| Explicit version | `--version 1.4.0` | Promote exactly this version/tag. |
| Prior run | `--from-run <id>` | Promote what a recorded run published (reads `dist/run-<id>/report.json`). `--from-run` is the most precise: it moves exactly the revisions that run uploaded, per its recorded evidence. |

An explicit `--version` or `--from-run` that matches **no** revision on any
configured snap is a hard failure with a non-zero exit — you asserted a
specific artifact exists and it does not. The default `Newest` selector against
an empty source track is the opposite: a skip with exit 0, because "nothing is
soaking this week" is a normal state for a scheduled promotion job.

`--dry-run` names the selector it resolved without contacting the store, so it
reports the plan for a version whether or not that version exists:

```console
$ anodizer promote --to stable --version 9.9.9 --publishers snapcraft --dry-run
   • (dry-run) would promote snapcraft myapp version 9.9.9 candidate→stable
   • snapcraft: 9.9.9→stable (dry-run)
```

The miss therefore appears on the live run, in the same two-part shape as the
rejected-channel example above: a `• snapcraft: … (failed: …)` result line
naming the version that matched no revision, then the aggregate
`1 publisher(s) failed to promote: snapcraft` and a non-zero exit.

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

```console
$ anodizer promote --to stable --dry-run
   • (dry-run) would promote snapcraft app newest candidate→stable
   • (dry-run) would promote npm newest next→latest
   • (dry-run) would promote docker ghcr.io/acme/app:edge → ghcr.io/acme/app:latest
   • (dry-run) would flip github release newest on acme/app (prerelease→stable)
   • snapcraft: candidate→stable (dry-run)
   • npm: next→latest (dry-run)
   • docker: 1 image(s) edge→latest (dry-run)
   • github: 1 release(s) prerelease→stable (dry-run)
```

The snapcraft dry-run names the **selector**, not a concrete revision:
resolving one needs a `snapcraft list-revisions` round-trip, and `--dry-run`
deliberately runs no external command and needs no credential.

Run the dry-run first whenever you are unsure which artifact the selector
resolves to.
