+++
title = "Selecting publishers"
description = "Run a tailored subset of publishers with the --publishers allowlist and the unified --skip denylist"
weight = 0
template = "docs.html"
+++

Anodizer runs every configured publisher by default. Two flags narrow that set when
you want a targeted release — `--publishers` (an allowlist) and `--skip` (a denylist).
Both are available on `release`, `publish`, `continue`, and `check config`, and both
accept the same publisher vocabulary, so a selection that works on one command works on
all of them. `continue` resumes a stalled release through the same publish pipeline, so
it validates and honors the selectors identically — a typo there is rejected before any
publisher runs, the same one-way-door guard the other commands carry.

## Allowlist vs denylist

| Flag | Role | Effect when set |
|------|------|-----------------|
| `--publishers <a,b,…>` | allowlist | Only the named publishers run. Every other publisher — including the irreversible publish stages `blob`, `snapcraft-publish`, `docker`, `docker-sign`, and `announce` (the last broadcasts to webhooks/Slack/Twitter/Mastodon/Bluesky) — is deselected. (The `release` stage that creates the GitHub release is the substrate the others depend on and is governed by `--skip=release` only; see the valid-names note below.) |
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
| `pypi` | publisher only |
| `schemastore` | publisher only |
| `mcp` | publisher only |
| `upstream-aur` | publisher only |
| `blob` | the publish stage `blob` (object-store upload) |
| `snapcraft-publish` | the publish stage `snapcraft-publish` (Snap Store upload) |
| `docker` | the publish stage `docker` (image-registry push) |
| `docker-sign` | the publish stage `docker-sign` (cosign signatures to the registry) |
| `announce` | the publish stage `announce` (irreversible external broadcasts — webhooks, Slack, Twitter, Mastodon, Bluesky) |

A name in the **yes** rows resolves the same whether you reach it through `--skip` (which
also accepts stage tokens) or `--publishers`. The **publisher only** names have no stage
of their own, so before this selection surface the only way to gate them was per-block
config — now they honor `--skip`/`--publishers` uniformly at dispatch time.

The last five names — `blob`, `snapcraft-publish`, `docker`, `docker-sign`, `announce` —
are publish **stages** rather than dispatch publishers, but each performs an external,
irreversible push (an object store, the Snap Store, an image registry, registry
signatures, or — for `announce` — broadcasts to webhooks/Slack/Twitter/Mastodon/Bluesky).
They are therefore governed by the same selectors: `--publishers cargo` deselects them, and
`--publishers blob` allow-lists a blob-only upload. Like `homebrew`, `announce` depends on
the release substrate (it reads the release URL) yet is itself a leaf, so the allowlist
governs it just like the others. The one exception is the `release` stage that creates the
GitHub/GitLab/Gitea release: it is the substrate every other publisher depends on
(manifests reference its assets; announce needs the release URL), so it is governed by
`--skip=release` only and is never dropped by a `--publishers` allowlist.

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

## Artifact eligibility

The install-aggregator publishers — Homebrew formulae, Homebrew casks, `nix`,
`krew`, `npm`, and `aur` — each package only the archives whose operating system
they can actually install. anodizer selects those archives for you; you never
list them by hand.

| Aggregator | Accepts | Excludes |
|------------|---------|----------|
| `homebrew`, `homebrew` casks, `nix`, `krew` | genuine macOS targets (`*-apple-darwin`, `darwin-universal`) and the Linux archives each supports | Apple **non-macOS** targets (`aarch64-apple-ios`, `*-tvos`, `*-watchos`) — Homebrew/nix/krew cannot install those, so they never appear in the emitted formula or manifest |
| `npm` | every OS npm's `os` field represents: `linux`, `darwin` (genuine macOS only), `win32`, `freebsd`, `openbsd`, `netbsd`, `aix`, `android` — the broadest coverage of any publisher | Apple **non-macOS** targets (same exclusion as above — npm has no `ios` `os` value) and any target npm has no os/arch mapping for (e.g. `darwin-universal`) |
| `aur` | Linux archives only | everything non-Linux |

An `aarch64-apple-ios` build in your target list is fine — it still builds and
uploads as a release asset. It is simply **not** folded into a Homebrew cask or
an npm package, because those installers would target the wrong OS.

### No eligible archive is a hard error, never a silent skip

If a configured install aggregator finds **no** archive it can install, anodizer
**fails the release** rather than emitting an empty or wrong-OS artifact:

```bash
$ anodizer release
...
Error: aur: no linux archives matched filters for 'myapp' — PKGBUILD would have
placeholder URL and empty sha256. Check your archive configuration and aur
filters (ids=<none>, amd64_variant=<default v1>, arm_variant=7 [hardcoded]). At
least one linux Archive artifact must match.
```

This closes a failure-hiding gap where a misconfigured build (say, an `aur`
block with only macOS targets configured) would otherwise ship an installable
file that installs nothing or points at the wrong platform. Fix the target list
or remove the publisher, then re-run. (On a sharded determinism build the check
is per-shard — see [Artifact validation](./validation.md) and
[Determinism](../advanced/determinism.md#emission-validate-on-sharded-builds).)

## Worked examples

### Publish a single channel from a finished build

`anodizer publish` reuses artifacts already in `dist/` and runs only the publish
pipeline. Pair it with `--publishers` to release one channel without re-running the rest:

```bash
$ anodizer publish --publishers npm
   • publishing npm
   • skipped cargo — not in --publishers allowlist
   • skipped homebrew — not in --publishers allowlist
   …
```

### Exclude one publisher from a full release

```bash
$ anodizer release --skip npm
   • skipped npm — excluded via --skip
   …    # every other configured publisher runs
```

### Tailor a release to two publishers

```bash
$ anodizer release --publishers cargo,homebrew
   • publishing cargo
   • publishing homebrew
   • skipped npm — not in --publishers allowlist
   • skipped dockerhub — not in --publishers allowlist
   …
```

### Two deselect outputs: the dispatch line and the summary line

A deselected publisher is never silent. anodizer emits the deselect line at
two points in the run — **in-flight** (as the publish pipeline reaches the
publisher) and again in the **publish summary** — with the **same**
distinguished wording at both. The two outputs differ only in *phase*, not in
text: both come from the one `deselected_reason` source, so they always name
the same cause.

| Wording (both phases) | When it applies |
|---|---|
| `skipped <name> — excluded via --skip` | The publisher was named in the `--skip` denylist. |
| `skipped <name> — not in --publishers allowlist` | A non-empty `--publishers` allowlist was given and this publisher was not in it. |

The line is **distinguished**: it names the exact cause so you can confirm a
publisher was turned off the way you intended. When both selectors apply to one
publisher, `--skip` wins and the line reports the denylist cause.

The five publish **stages** (`blob`, `snapcraft-publish`, `docker`,
`docker-sign`, `announce`) run outside the dispatch loop, so a deselected one
emits a single line at its stage — `skipped blob — not in --publishers
allowlist` / `skipped docker — excluded via --skip` — rather than the
in-flight-plus-summary pair. The wording is identical, so you still see exactly
which selector turned it off.

```bash
$ anodizer release --skip npm
   • skipped npm — excluded via --skip   # in-flight
   …    # every other configured publisher runs
   • skipped npm — excluded via --skip   # summary (same wording)
```

```bash
$ anodizer release --publishers cargo
   • publishing cargo
   • skipped npm — not in --publishers allowlist   # in-flight
   …
   • skipped npm — not in --publishers allowlist   # summary (same wording)
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
