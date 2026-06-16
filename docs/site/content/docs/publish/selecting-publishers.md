+++
title = "Selecting publishers"
description = "Run a tailored subset of publishers with the --publishers allowlist and the unified --skip denylist"
weight = 0
template = "docs.html"
+++

Anodizer runs every configured publisher by default. Two flags narrow that set when
you want a targeted release — `--publishers` (an allowlist) and `--skip` (a denylist).
Both are available on `release`, `publish`, and `check config`, and both accept the
same publisher vocabulary, so a selection that works on one command works on all three.

## Allowlist vs denylist

| Flag | Role | Effect when set |
|------|------|-----------------|
| `--publishers <a,b,…>` | allowlist | Only the named publishers run. Everything else is deselected. |
| `--skip <a,b,…>` | denylist | The named stages **and** publishers are removed; everything else runs. |

Both flags are comma-separated and may be repeated:

```bash
anodizer release --publishers cargo,homebrew
anodizer release --publishers cargo --publishers homebrew   # equivalent
anodizer release --skip npm,dockerhub
```

### Precedence: `--skip` always wins

When a publisher appears in both lists, `--skip` removes it. This makes the denylist a
hard veto you can layer on top of an allowlist:

```bash
# Allow cargo + homebrew, but veto homebrew anyway → only cargo runs.
anodizer release --publishers cargo,homebrew --skip homebrew
```

### Empty means "all configured"

Omitting both flags (or passing empty values) runs every configured publisher — the
default release behavior is unchanged:

```bash
anodizer release           # every configured publisher runs
```

## The valid publisher names

`--publishers` and `--skip` accept these publisher names (the canonical token for each
publisher is its own name — anodizer derives the set from the publisher registry, so it
never drifts):

| Publisher name | Also a stage name? |
|----------------|--------------------|
| `cargo` | yes |
| `homebrew` | yes |
| `scoop` | yes |
| `nix` | yes |
| `aur` | yes |
| `krew` | yes |
| `winget` | yes |
| `chocolatey` | yes |
| `github-release` | the stage token is `release` |
| `npm` | publisher only |
| `dockerhub` | publisher only |
| `uploads` | publisher only |
| `artifactory` | publisher only |
| `cloudsmith` | publisher only |
| `gemfury` | publisher only |
| `schemastore` | publisher only |
| `mcp` | publisher only |
| `upstream-aur` | publisher only |

A name in the **yes** rows resolves the same whether you reach it through `--skip` (which
also accepts stage tokens) or `--publishers`. The **publisher only** names have no stage
of their own, so before this selection surface the only way to gate them was per-block
config — now they honor `--skip`/`--publishers` uniformly at dispatch time.

### Canonical names: `homebrew`, not `brew`; `chocolatey`, not `choco`

The selection vocabulary uses each publisher's full name. The short forms are rejected
with a loud error so a typo can never silently widen or narrow a release:

```bash
$ anodizer release --skip brew
Error: invalid --skip value(s): brew. Valid options: …, homebrew, …, chocolatey, …

$ anodizer release --publishers crates
Error: invalid --publishers value(s): crates. Valid publishers: cargo, …
```

Use `homebrew`, `chocolatey`, and `cargo` (the canonical names from the table above).

## Worked examples

### Publish a single channel from a finished build

`anodizer publish` reuses artifacts already in `dist/` and runs only the publish
pipeline. Pair it with `--publishers` to release one channel without re-running the rest:

```bash
$ anodizer publish --publishers npm
   • publishing npm
   • skipping cargo — deselected via --skip / --publishers
   • skipping homebrew — deselected via --skip / --publishers
   …
```

### Exclude one publisher from a full release

```bash
$ anodizer release --skip npm
   • skipping npm — deselected via --skip / --publishers
   …    # every other configured publisher runs
```

### Tailor a release to two publishers

```bash
$ anodizer release --publishers cargo,homebrew
   • publishing cargo
   • publishing homebrew
   • skipping npm — deselected via --skip / --publishers
   • skipping dockerhub — deselected via --skip / --publishers
   …
```

### Two deselect outputs: the dispatch line and the summary line

A deselected publisher is never silent. anodizer emits **two** distinct
operator-visible lines for it, at two different points in the run:

| Phase | Line | When |
|---|---|---|
| **Dispatch** (in-flight) | `skipping <name> — deselected via --skip / --publishers` | As the pipeline walks the publisher list and reaches a deselected one. |
| **Publish summary** (final) | `skipped <name> — excluded via --skip` | The publisher was named in the `--skip` denylist. |
| **Publish summary** (final) | `skipped <name> — not in --publishers allowlist` | A non-empty `--publishers` allowlist was given and this publisher was not in it. |

The dispatch line is uniform — it states only that the publisher was
deselected. The summary line is **distinguished**: it names the exact cause
so you can confirm a publisher was turned off the way you intended. When both
selectors apply to one publisher, `--skip` wins and the summary reports the
denylist cause.

```bash
$ anodizer release --skip npm
   • skipping npm — deselected via --skip / --publishers   # dispatch
   …    # every other configured publisher runs
   • skipped npm — excluded via --skip                     # summary
```

```bash
$ anodizer release --publishers cargo
   • publishing cargo
   • skipping npm — deselected via --skip / --publishers   # dispatch
   …
   • skipped npm — not in --publishers allowlist           # summary
```

## Validating a selection ahead of release

`check config --publishers <names>` validates the selection against the **configured**
publishers without running anything. Naming a publisher you forgot to configure is an
error, so you catch a selection mistake before tag time rather than at release time:

```bash
$ anodizer check config --publishers cargo
   • validating configuration
   • Config is valid.

$ anodizer check config --publishers npm
Error: publisher 'npm' named in --publishers is not configured (no npm publish block)
```

The same loud error guards a typo here as on `release`/`publish`:

```bash
$ anodizer check config --publishers crates
Error: invalid --publishers value(s): crates. Valid publishers: cargo, …
```

## Typos fail loud — one-way-door safety

Several publishers push to one-way-door registries (crates.io, Cloudsmith, Chocolatey,
winget, AUR). A misread selection must never quietly publish nothing or everything, so
anodizer rejects any unknown token with a nonzero exit before dispatch begins:

```bash
$ anodizer release --publishers homebrwe   # typo
Error: invalid --publishers value(s): homebrwe. Valid publishers: cargo, …
$ echo $?
1
```

Because the error fires before the publish pipeline runs, a typo can never reach a
registry — you fix the name and re-run.

## See also

- [`required:`](./_index.md) — control which publisher failures fail the release.
- [NPM](./npm.md) — npm provenance requires a GitHub-hosted runner, so CI peels npm
  onto a separate github-hosted job while the rest of the release runs elsewhere.
