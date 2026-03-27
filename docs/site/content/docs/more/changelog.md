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
| `use` | string | `git` | Source: `git` (commit parsing) or `github-native` (GitHub's generated notes) |
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
