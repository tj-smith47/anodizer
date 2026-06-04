+++
title = "Changelog"
description = "Generate changelogs from git commits"
weight = 50
template = "docs.html"
+++

The changelog stage generates release notes from git commits between the previous tag and the current tag. The standalone `anodizer changelog` command is the unified front door for that engine: it refreshes the in-repo `CHANGELOG.md`, emits a GitHub-release body, or dumps structured JSON — all from the same grouped-and-filtered commit history the release pipeline uses.

## The `anodizer changelog` command

```text
anodizer changelog [<tag>|<range>] [--format keep-a-changelog|release-notes|json] [--write] [--crate <name>] [--snapshot]
```

| Flag / arg | Default | Effect |
|------------|---------|--------|
| `[<tag>\|<range>]` | last-tag..HEAD | Commit range to render (see [Selecting a range](#selecting-a-range)) |
| `--format` | `keep-a-changelog` | Output shape: refresh the `[Unreleased]` section, a GitHub-release body, or JSON |
| `--write` | off (preview) | Apply the regenerated `[Unreleased]` to the configured `CHANGELOG.md` in place (`keep-a-changelog` only) |
| `--crate <name>` | all selected crates | Restrict to one crate in a workspace |
| `--snapshot` | off | Render as a snapshot release (`release-notes` only) |

There is no `--output`/`-o` (redirect stdout instead), no `--from`/`--to` (use the
positional range), and no `check changelog` subcommand.

### Selecting a range

The positional arg drives every format identically — the same arg surfaces the
same commits whether you render `keep-a-changelog`, `release-notes`, or `json`.

| Arg | Lower bound | Renders |
|-----|-------------|---------|
| _(omitted)_ | each crate's last release tag | the pending `[Unreleased]` window (since the last release) |
| `..` | none — start of history | full history → HEAD |
| `..<ref>` | none — start of history | full history → `<ref>` |
| `<from>..` | `<from>` | `<from>` → HEAD |
| `<from>..<to>` | `<from>` | `<from>` → `<to>` |
| `<tag>` | the tag's predecessor | exactly that release's entries |

```bash
anodizer changelog                       # omit → each crate's pending window (since last release)
anodizer changelog ..                    # full history → HEAD
anodizer changelog ..v1.2.0              # full history → v1.2.0
anodizer changelog v1.0.0..v1.2.0        # explicit range
anodizer changelog v1.2.0                # one release's slice: predecessor..v1.2.0
```

An **empty lower bound** (a leading `..`) always means "from the beginning of
history." Omitting the arg entirely is different: it is the *pending* window,
bounded at each crate's last release tag. So `anodizer changelog ..` (full
history) and `anodizer changelog` (since last release) are distinct — and `..`
and `..HEAD` are the same (both full history to HEAD).

A single `<tag>` resolves the owning crate from its tag prefix
(`core-v0.2.0` → the `core` crate) and bounds the range at the predecessor tag —
the tag immediately below it in that crate's semver-sorted list — so you get
exactly that release's entries. A tag that is the earliest in its series has no
predecessor, so it falls back to full history up to that tag.

### `--format keep-a-changelog` (default) — refresh `[Unreleased]`

Regenerates the `## [Unreleased]` section of the configured `CHANGELOG.md` in
Keep-a-Changelog form. A bare command previews to stdout and writes nothing:

```text
$ anodizer changelog
## [Unreleased]

### Features

* a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0 add config validation

### Bug Fixes

* e4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c0d1e2f3 handle empty target list
```

By default each line is `* {{ SHA }} {{ Message }}` with the full hash — the
conventional-commit `feat:` / `fix:` prefix is stripped into the group heading.
Set `abbrev` to truncate the hash and `format` to reshape the line (see the
[config fields](#changelog-config-fields) below).

`--write` applies that regenerated `[Unreleased]` to the file in place. It
preserves every released section and the compare-link footer — it rewrites
**only** `[Unreleased]`, and it does **not** promote/roll `[Unreleased]` to a
dated `## [x.y.z]` version (that's [`anodizer tag --changelog`](@/docs/advanced/auto-tagging.md)):

```text
$ anodizer changelog --write
changelog: refreshed CHANGELOG.md [Unreleased]
```

`--write` is valid only with `--format keep-a-changelog`; pairing it with
`release-notes`/`json` errors (those stream to stdout for you to redirect).

### `--format release-notes` — GitHub release body to stdout

Emits the grouped-bullet markdown anodizer posts as the GitHub release body.
Redirect stdout to capture it:

```text
$ anodizer changelog --format release-notes
## Changelog

### Features

* a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0 add config validation

### Bug Fixes

* e4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c0d1e2f3 handle empty target list
```

```bash
anodizer changelog --format release-notes > NOTES.md            # capture to a file
anodizer changelog v1.2.0 --format release-notes                # body for one release
anodizer changelog --snapshot --format release-notes            # preview the pending window pre-tag
```

To preview the pending window *before* the release is tagged — when HEAD is
ahead of the last release tag — pass `--snapshot`. Without it, release-notes
requires HEAD to point at a tag and errors otherwise. The snapshot preview is
bounded at the last release tag (the same since-last-release window as
`keep-a-changelog` and `json`), not full history.

### `--format json` — structured array to stdout

Emits a JSON array, one object per selected crate, sorted by crate name. Each
object is `{ crate, from, to, groups }`, where every group carries `entries`
(with `summary`, `sha`, `full_sha`, `authors`) and nested `subgroups`:

```text
$ anodizer changelog v1.2.0 --format json
[
  {
    "crate": "myapp",
    "from": "v1.1.0",
    "to": "v1.2.0",
    "groups": [
      {
        "title": "Features",
        "entries": [
          {
            "summary": "add config validation",
            "sha": "a1b2c3d",
            "full_sha": "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0",
            "authors": ["Jane Dev"]
          }
        ],
        "subgroups": []
      }
    ]
  }
]
```

`from` is `null` for full history; `to` resolves to `HEAD` when the range is
unbounded.

### End-to-end: preview → write → edit → tag

The standalone command and the tag-time promotion compose into one flow:

```bash
anodizer changelog              # 1. preview the pending [Unreleased]
anodizer changelog --write      # 2. refresh CHANGELOG.md's [Unreleased] in place
# 3. hand-edit the [Unreleased] section, then commit it
anodizer tag --changelog        # 4. promote [Unreleased] → [x.y.z] - <date>,
                                #    preserving your committed edits verbatim
```

Step 4 is opt-in via `--changelog`; see
[Auto-Tagging](@/docs/advanced/auto-tagging.md) for the tag-time refresh.

## Minimal config

Changelog generation works with no config — it collects all commits since the last tag.

## Changelog config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `sort` | string | `asc` | Sort order: `asc` or `desc` |
| `use` | string | `git` | Source: `git` (commit parsing), `github` (fetch commits via GitHub API), or `github-native` (GitHub's generated notes) |
| `abbrev` | int | `0` | Hash length: `0` = full SHA, `N` = truncate to N chars, `-1` = omit the hash |
| `skip` | bool/template | `false` | Skip changelog generation (alias: `disable`) |
| `header` | string | none | Text prepended to changelog |
| `footer` | string | none | Text appended to changelog |
| `filters.exclude` | list | none | Regex patterns to exclude commits |
| `filters.include` | list | none | Regex patterns to include (whitelist) |
| `groups` | list | none | Group commits by pattern |

## Commit grouping

Group conventional commits by type:

```yaml
changelog:
  groups:
    - title: "Features"
      regexp: "^feat"
      order: 0
    - title: "Bug Fixes"
      regexp: "^fix"
      order: 1
    - title: "Documentation"
      regexp: "^docs"
      order: 2
    - title: "Other"
      regexp: ".*"
      order: 99
```

## Filtering commits

```yaml
changelog:
  filters:
    exclude:
      - "^chore"
      - "^ci"
      - "Merge pull request"
```

## Changelog destination

In a workspace, `changelog:` chooses **where** released sections land: a shared
root `CHANGELOG.md`, a per-crate `crates/<name>/CHANGELOG.md`, or both. Two
fields drive it:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `files.per_crate` | bool | `false` | Write each crate's section to `crates/<name>/CHANGELOG.md` |
| `files.root` | block | on unless `files.per_crate: true` | Write the shared root `CHANGELOG.md`; presence forces it on |
| `files.root.chronology` | string | `date` | Section ordering in a multi-track root: `date` or `tag` |
| `files.root.crates` | list | all | Which crates contribute a section to the root |

The resolved destination follows one rule: the root is on when a `files.root:`
block is present **or** `files.per_crate` is not `true`. That yields three
outcomes, each shown below.

**Root only** (the default — a bare block aggregates into the workspace root):

```yaml
changelog: {}            # root CHANGELOG.md
```

**Per-crate files** (each crate keeps its own changelog, no root):

```yaml
changelog:
  files:
    per_crate: true      # crates/<name>/CHANGELOG.md, one per crate
```

**Both** (per-crate files *and* the shared root):

```yaml
changelog:
  files:
    per_crate: true
    root: {}             # crates/<name>/CHANGELOG.md AND root CHANGELOG.md
```

Single-crate, lockstep, and **shared-prefix flat-crate** roots are **flat**: one
aggregated section per release covering the whole workspace (the
[flat-aggregate shape](#flat-aggregate-one-shared-tag-one-flat-section) below
covers the last case). `files.root.crates` filters which crates contribute a
section to the root:

```yaml
changelog:
  files:
    root:
      crates: ["core", "cli"]   # only these crates appear in the root changelog
```

### Commit scoping

Commit scoping is **derived** — no config required. A per-crate track collects
commits that touch its own crate directory; the root aggregate spans every
crate directory plus the workspace manifests (`Cargo.toml`, `Cargo.lock`). Each
crate's changelog stays focused on its own history automatically.

`changelog.paths` is an optional advanced **intersect** filter that narrows the
derived scope further — list extra path globs to additionally restrict which
commits a track considers. It only subtracts from the automatic per-directory
scope; you never need it to get correct per-crate or aggregate scoping.

### Workspace shapes at a glance

anodizer classifies a repo into one of four shapes from its config + Cargo
metadata, and the changelog (and tag, and `bump --commit`) follow from the
shape. The config signal is the only thing you set:

| Shape | Config signal | Tag behavior | Changelog shape |
|-------|---------------|--------------|-----------------|
| **Single** | one crate, or no config | one `v*` tag | one flat section |
| **Lockstep** | `[workspace.package].version` in root `Cargo.toml` | one shared `v*` tag | one flat section |
| **Flat-aggregate** | flat `crates:` list, every `tag_template` resolves to the **same** prefix, per-crate `[package].version` | one shared `v*` tag | one flat section |
| **Multi-track** | flat `crates:` list (or `workspaces:`) with **distinct** tag prefixes (`core-v`, `cli-v`) | per-crate tags | `### <crate>` subsection per track |

### Flat-aggregate: one shared tag, one flat section

A flat `crates:` list whose members **all** resolve to the same tag prefix
(every `tag_template` is `v{{ Version }}`, or all `acme-v{{ Version }}`) releases
in lockstep: one tag namespace can't hand `v0.2.0` to two crates independently.
anodizer treats this exactly like a lockstep workspace — **one shared `v*` tag,
one flat `[Unreleased]`/released section** — even though each crate carries its
own `[package].version` (rather than a single `[workspace.package].version`):

```yaml
crates:
  - name: core
    path: crates/core
    tag_template: "v{{ Version }}"   # same prefix
  - name: cli
    path: crates/cli
    tag_template: "v{{ Version }}"   # as every other member
```

```text
$ anodizer changelog
## [Unreleased]

### Features

* a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0 add config validation
```

No `### core` / `### cli` graft appears — the section is flat, identical to a
lockstep root. `anodizer tag --changelog` and `anodizer bump --commit --changelog`
promote that one flat section under a single `## [v0.2.0] - <date>` heading. To
get per-crate `### <crate>` subsections instead, give each crate a **distinct**
tag prefix (the [multi-track](#multi-track-root-subsections) shape below).

### Coherence: members must agree on `[package].version`

In a flat-aggregate one tag carries one version, so every member's
`[package].version` must **agree**. If they diverge, `changelog`, `tag`, and
`bump` all **error before doing anything** — no partial tag, no partial
changelog write:

```yaml
crates:
  - name: core
    path: crates/core
    tag_template: "v{{ Version }}"   # crates/core/Cargo.toml → version = "0.2.0"
  - name: cli
    path: crates/cli
    tag_template: "v{{ Version }}"   # crates/cli/Cargo.toml  → version = "0.3.0"
```

```text
$ anodizer changelog
Error: crates 'core' (0.2.0), 'cli' (0.3.0) share tag prefix 'v' but set
different [package].version values; one tag can't carry two versions. For
lockstep set [workspace.package].version; for independent releases give each
crate a distinct tag_template prefix.
```

Two ways to resolve it, depending on intent:

- **Lockstep** — the crates truly ship together under one version: set
  `[workspace.package].version` in the root `Cargo.toml` and have each member
  inherit it (`version.workspace = true`). The repo is then a genuine lockstep
  workspace.
- **Independent releases** — the crates ship on their own cadences: give each a
  distinct `tag_template` prefix (`core-v{{ Version }}`, `cli-v{{ Version }}`),
  promoting the repo to the [multi-track](#multi-track-root-subsections) shape.

A member with no literal `[package].version` (e.g. `version.workspace = true`,
or a virtual manifest) is skipped by the check — it has no concrete version to
disagree with.

### Multi-track root subsections

When crates release on independent tag tracks (e.g. `core-v*` and `cli-v*`), the
root `CHANGELOG.md` holds a `### <crate>` subsection per track under
`## [Unreleased]`. Tagging one track promotes **only that crate's** subsection
to a released `## [<tag>] - <date>` heading — regrouped under your `groups:`
headings — and leaves every other track's subsection in place.

Before — curate each track's entries under its own subsection:

```markdown
## [Unreleased]

### core
- add the retry budget

### cli
- new `--watch` flag

[Unreleased]: https://github.com/acme/proj/compare/core-v0.1.0...HEAD
```

After `anodizer tag` on the `core` track — `### core` is promoted, `### cli`
stays untouched, and the compare footer rolls to the `core` tag:

```markdown
## [Unreleased]

### cli
- new `--watch` flag

## [core-v0.2.0] - 2026-06-03

### Features
- add the retry budget

[Unreleased]: https://github.com/acme/proj/compare/core-v0.2.0...HEAD
[core-v0.2.0]: https://github.com/acme/proj/compare/core-v0.1.0...core-v0.2.0
```

### Chronology: `date` vs `tag`

`files.root.chronology` orders the released sections in a multi-track root. Given two
tracks `core-v*` and `cli-v*`, the same set of releases renders differently:

| `chronology: date` (default) | `chronology: tag` |
|---|---|
| Newest **ship date** on top, tracks interleaved | Clustered by tag-prefix, semver-descending within a cluster |

```markdown
# chronology: date — interleaved by release date
## [cli-v0.4.0] - 2026-06-03
## [core-v0.2.0] - 2026-06-01
## [cli-v0.3.0] - 2026-05-20
```

```markdown
# chronology: tag — clustered per crate, semver-desc
## [cli-v0.4.0] - 2026-06-03
## [cli-v0.3.0] - 2026-05-20
## [core-v0.2.0] - 2026-06-01
```

## Full example

```yaml
changelog:
  sort: desc
  header: |
    ## Changelog
  filters:
    exclude:
      - "^chore"
      - "^ci"
  groups:
    - title: "Features"
      regexp: "^feat"
      order: 0
    - title: "Bug Fixes"
      regexp: "^fix"
      order: 1
    - title: "Other"
      regexp: ".*"
      order: 99
```
