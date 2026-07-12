+++
title = "Homebrew Core"
description = "Bump an existing formula in Homebrew/homebrew-core with a fork-based pull request"
weight = 4
template = "docs.html"
+++

Anodizer bumps an **existing formula in [`Homebrew/homebrew-core`](https://github.com/Homebrew/homebrew-core)** (or any formula repository you point it at) to your new release — entirely through the GitHub API. It reads the formula file, rewrites its `url` / `sha256` / `version` stanzas (or `tag:` / `revision:` for git-based formulae), commits the change to a branch on your fork, and opens a pull request. There is no `git clone` (the core repo is multiple gigabytes) and no `brew` invocation.

This mirrors the semantics of [`brew bump-formula-pr`](https://docs.brew.sh/Manpage#bump-formula-pr-options-formula) and the [`mislav/bump-homebrew-formula-action`](https://github.com/mislav/bump-homebrew-formula-action) GitHub Action — so a project migrating from that action drops the workflow step and keeps its existing secret. GoReleaser has no homebrew-core formula bump (its `brews` pipe only pushes to a personal tap); anodizer goes beyond it here.

## homebrew-core vs. the Homebrew tap

Two different Homebrew surfaces, two different publishers:

| Use | Publisher | What it does |
|---|---|---|
| Bump a formula **already accepted into** `Homebrew/homebrew-core` | `homebrew_cores` (this page) | Rewrites the upstream formula and opens a PR against the core repo |
| Distribute from **your own tap** (`youruser/homebrew-tap`) | [`homebrew` / `homebrew_casks`](./homebrew.md) | Generates the formula/cask and pushes it to a tap you control |

Most projects use a tap. Reach for `homebrew_cores` only once your tool is popular enough to have been merged into homebrew-core, where every release needs a bump PR. The two can coexist: ship to your tap for immediacy and open a core bump PR for the wider audience.

## Classification

| Group | Required (default) | Rollback | One-way door | Token |
|-------|--------------------|----------|--------------|-------|
| Submitter | `false` | close the opened PR | **NEVER** | `HOMEBREW_CORE_GITHUB_TOKEN` / `COMMITTER_TOKEN` / `ANODIZER_GITHUB_TOKEN` / `GITHUB_TOKEN` |

The bump is **fully reversible** — it opens a pull request, which a triggered rollback closes (`--rollback-only --from-run`). Nothing about it consumes a version or crosses a one-way door, so `required` defaults to `false`: a failed bump PR is fixed by hand and must never abort the release.

## Quick start

```yaml
homebrew_cores:
  - name: my-tool
```

Run with `HOMEBREW_CORE_GITHUB_TOKEN` exported (a token that can fork `Homebrew/homebrew-core` and open PRs). Everything else is derived: the formula name falls back to the crate name, the target repository defaults to `Homebrew/homebrew-core`, the formula path defaults to the sharded core layout, and the download URL defaults to the GitHub source tarball for the release tag — which anodizer downloads and hashes to fill `sha256`.

```console
$ anodizer release
  • processing homebrew-core bump 'homebrew_cores[0]'
  • bumped formula my-tool to 1.2.3 — opened Homebrew/homebrew-core#12345 (https://github.com/Homebrew/homebrew-core/pull/12345)
```

## Configuration

```yaml
homebrew_cores:
  - id: main                                # CLI selector (--id=main)
    ids: [my-tool]                          # crate scoping (name default + source repo)
    name: my-tool                           # formula name (templated; default: crate name)
    repository:                             # default: Homebrew/homebrew-core
      owner: Homebrew
      name: homebrew-core
      branch: master                        # default: repo's default branch
      token: "{{ .Env.HOMEBREW_CORE_GITHUB_TOKEN }}"   # templated
      pull_request:
        draft: false                        # open the bump PR as a draft
        body: "Bump {{ .ProjectName }} to {{ .Version }}"  # templated PR body
    path: "Formula/m/my-tool.rb"            # templated; default: sharded then flat layout
    download_url: "https://github.com/acme/my-tool/archive/refs/tags/{{ .Tag }}.tar.gz"  # templated
    sha256: "{{ .Env.SRC_SHA256 }}"         # templated; default: download + hash download_url
    commit_msg_template: "my-tool {{ .Version }}"  # templated; default: "<formula> <version>"
    direct_commit: false                    # bool or template; personal repos only
    skip: false                             # bool or template
    if: "{{ not .IsNightly }}"              # template-conditional gate
    required: false                         # default; failure does not abort the release
    retain_on_rollback: false               # default; rollback closes the PR
```

| Field | Templated | Default | Purpose |
|---|---|---|---|
| `id` | — | — | CLI selector for `--id=...` |
| `ids` | — | primary crate | Crate scoping: names the formula default and picks the source repo for the default `download_url` |
| `name` | **yes** | scoped crate → primary crate → project name | Formula name |
| `repository` | — | `Homebrew/homebrew-core` | Target formula repository |
| `repository.branch` | — | repo's default branch | Base branch the PR targets |
| `repository.token` | **yes** | token ladder (below) | Auth token override |
| `repository.pull_request.draft` | — | `false` | Open the bump PR as a draft |
| `repository.pull_request.body` | **yes** | generated "Bump …" body | PR description |
| `path` | **yes** | sharded `Formula/<letter>/<name>.rb`, then flat `Formula/<name>.rb` | Formula file path in the repo |
| `download_url` | **yes** | GitHub source tarball for the tag | URL written into the formula's `url` stanza |
| `sha256` | **yes** | download + hash `download_url` | Source-archive digest |
| `commit_msg_template` | **yes** | `"<formula> <version>"` | Commit message / PR title |
| `direct_commit` | **yes** (bool/template) | `false` | Commit straight to the base branch (personal repos only) |
| `skip` / `if` | **`if` yes** | — | Entry gating (bool/template; falsy `if` skips). `skip` also accepts the legacy `disable:` spelling via serde alias |
| `required` | — | `false` | Whether failure fails the release |
| `retain_on_rollback` | — | `false` | Leave the opened PR in place on rollback |

## What gets rewritten

Anodizer never parses the formula as Ruby — it rewrites the small, rigidly-formatted stanzas Homebrew's own audit tooling enforces:

**Archive form** (the common case): the `url`, the standalone source `sha256`, and the explicit `version` stanza (when present) are rewritten to the new release.

```ruby
  url "https://github.com/acme/my-tool/archive/refs/tags/v1.2.3.tar.gz"   # ← rewritten
  sha256 "22…22"                                                          # ← rewritten
  version "1.2.3"                                                         # ← rewritten (if present)
```

**Git form**: when the `url` stanza carries `tag:` / `revision:` fields, those are rewritten instead and the `sha256` is left alone (git-based formulae carry no source digest).

```ruby
  url "https://github.com/acme/my-tool.git",
      tag: "v1.2.3",                                                      # ← rewritten
      revision: "f0f0…"                                                   # ← rewritten
```

Bottle-block digests (`sha256 cellar: …`, `sha256 arm64_sonoma: "…"`) are **left untouched** — they carry a key before the digest and Homebrew's CI rebuilds bottles after the version bump.

## Fork + pull request flow

For `Homebrew/homebrew-core` (which never accepts direct pushes or same-repo bot branches), the bump always forks and opens a PR:

1. `GET /repos/Homebrew/homebrew-core` — resolve the base branch.
2. `GET …/contents/Formula/m/my-tool.rb` — read the current formula (sharded layout, falling back to flat).
3. `POST …/forks` — ensure the authenticated user's fork exists.
4. `POST …/git/refs` on the fork — create the `bump-<formula>-<version>` branch at the upstream base.
5. `PUT …/contents/…` on the fork — commit the rewritten formula.
6. `POST …/pulls` on upstream — open the pull request.

For a **personal formula repository** the token can push to, anodizer uses a same-repo bump branch (no fork) and still opens a PR.

## Direct commit

`direct_commit: true` commits the bump straight to the base branch instead of opening a PR — useful for a personal formula repo you fully control.

```yaml
homebrew_cores:
  - repository: { owner: myorg, name: homebrew-mine }
    direct_commit: true
```

This is **only honored for repositories you can push to**. A bump targeting `Homebrew/homebrew-core` is always forced through a fork + PR regardless of `direct_commit`, because homebrew-core never accepts direct pushes. A `direct_commit` bump has no PR, so a triggered rollback is warn-only (revert the commit by hand).

## Idempotency and re-runs

Re-running a release is safe — the bump is a no-op when it has already happened:

- **Formula already current** — if the formula's `url` / `tag:` / `version` already reference the new release, anodizer logs `formula … already at <version> — skipping (idempotent)` and opens nothing.
- **Open PR already exists** — if a bump branch from this run's head already has an open PR, anodizer logs `open PR already bumps … — skipping (idempotent)` rather than opening a duplicate.
- **Branch re-created** — a stale bump branch from a prior failed run is force-moved to the fresh upstream base, so the retried bump starts clean.

## Authentication

The bump needs a GitHub token that can fork the formula repository and open pull requests. Resolution ladder:

1. `homebrew_cores[].repository.token` (templated) when set;
2. `$HOMEBREW_CORE_GITHUB_TOKEN`;
3. `$COMMITTER_TOKEN` — the name [`mislav/bump-homebrew-formula-action`](https://github.com/mislav/bump-homebrew-formula-action) consumes, so a project migrating from that action keeps its existing secret;
4. the standard GitHub ladder (`$ANODIZER_GITHUB_TOKEN`, then `$GITHUB_TOKEN`).

Empty values are skipped at every link, so a blank `GITHUB_TOKEN` (GitHub Actions' shape for a missing secret) never masquerades as a real token.

### Migrating from `mislav/bump-homebrew-formula-action`

```yaml
# Before (GitHub Actions workflow step):
#   - uses: mislav/bump-homebrew-formula-action@v3
#     with:
#       formula-name: my-tool
#     env:
#       COMMITTER_TOKEN: ${{ secrets.COMMITTER_TOKEN }}

# After (.anodizer.yaml) — same secret, folded into the release:
homebrew_cores:
  - name: my-tool
```

## Preflight

`anodizer preflight` probes each active entry and *warns* (never blocks — the publisher defaults to `required: false`) when:

- no GitHub token is resolvable for the bump,
- the formula does not exist in the target repository (this publisher bumps an **existing** formula — submit the initial formula by hand first),
- the formula is already at the new version (the run path will skip it idempotently).

## Nightlies

The homebrew-core publisher skips nightly runs — a bump PR per night against a moderated public index is spam. It is listed with the tap `homebrew` publisher under the automatic nightly skips. See [Nightlies](./nightlies.md).
