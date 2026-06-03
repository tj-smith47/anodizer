+++
title = "Changelog"
description = "Generate changelogs from git commits"
weight = 50
template = "docs.html"
+++

The changelog stage generates release notes from git commits between the previous tag and the current tag.

## Minimal config

Changelog generation works with no config — it collects all commits since the last tag.

## Changelog config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `sort` | string | `asc` | Sort order: `asc` or `desc` |
| `use` | string | `git` | Source: `git` (commit parsing), `github` (fetch commits via GitHub API), or `github-native` (GitHub's generated notes) |
| `abbrev` | int | none | Truncate commit hashes to this length |
| `disable` | bool | `false` | Disable changelog generation |
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
| `per_crate` | bool | `false` | Write each crate's section to `crates/<name>/CHANGELOG.md` |
| `root` | block | on unless `per_crate: true` | Write the shared root `CHANGELOG.md`; presence forces it on |
| `root.chronology` | string | `date` | Section ordering in a multi-track root: `date` or `tag` |
| `root.crates` | list | all | Which crates contribute a section to the root |

The resolved destination follows one rule: the root is on when a `root:` block
is present **or** `per_crate` is not `true`. That yields three outcomes, each
shown below.

**Root only** (the default — a bare block aggregates into the workspace root):

```yaml
changelog: {}            # root CHANGELOG.md
```

**Per-crate files** (each crate keeps its own changelog, no root):

```yaml
changelog:
  per_crate: true        # crates/<name>/CHANGELOG.md, one per crate
```

**Both** (per-crate files *and* the shared root):

```yaml
changelog:
  per_crate: true
  root: {}               # crates/<name>/CHANGELOG.md AND root CHANGELOG.md
```

Single-crate and lockstep roots are **flat**: one aggregated section per release
covering the whole workspace. `root.crates` filters which crates contribute a
section to the root:

```yaml
changelog:
  root:
    crates: ["core", "cli"]   # only these crates appear in the root changelog
```

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

`root.chronology` orders the released sections in a multi-track root. Given two
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
