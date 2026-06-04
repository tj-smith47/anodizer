+++
title = "Git"
description = "Configure tag sorting, filtering, and version detection from git"
weight = 7
template = "docs.html"
+++

Anodizer detects the current version from git tags. The `git` section lets you control how tags are sorted and which tags are considered.

## Minimal config

```yaml
git:
  tag_sort: "-version:refname"
```

## Git config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `tag_sort` | string | `-version:refname` | How to sort tags. See [Tag sorting](#tag-sorting) below. Accepted: `-version:refname`, `-version:creatordate`, `semver`, `smartsemver` |
| `ignore_tags` | list | none | Glob patterns for tags to exclude from version detection (supports templates) |
| `ignore_tag_prefixes` | list | none | Prefixes for tags to exclude from version detection (supports templates) |
| `prerelease_suffix` | string | none | Suffix identifying pre-release tags for sort ordering |

## Tag sorting

Four sort modes are accepted:

| Value | Sort surface | When to use |
|-------|--------------|-------------|
| `-version:refname` (default) | git-delegated lexicographic version sort | The historical default; matches GoReleaser OSS |
| `-version:creatordate` | git-delegated by tag creation date, newest first | When tags are created out of version order and creation order is the source of truth |
| `semver` | Rust-side strict SemVer 2.0.0 sort | When you want pure-spec ordering with no shell-out; prereleases sort below their release per section 11 |
| `smartsemver` | Rust-side SemVer + prerelease filter | When you ship release tags after prerelease tags and want changelogs to skip the prereleases automatically |

Creation-date sort:

```yaml
git:
  tag_sort: "-version:creatordate"
```

Strict SemVer sort (bypasses git's native sort):

```yaml
git:
  tag_sort: "semver"
```

Smart SemVer sort — when the current run's `Version` is non-prerelease, prerelease tags are filtered out of the candidate list before picking the latest or previous tag:

```yaml
git:
  tag_sort: "smartsemver"
```

`smartsemver` prevents the common pitfall where shipping `v0.2.0` after a `v0.2.0-beta.3` tag would pick the beta as the previous tag and produce an empty changelog.

## Ignoring tags

Filter out tags that should not be considered for version detection:

```yaml
git:
  ignore_tags:
    - "nightly*"
    - "legacy-*"
    - "{{ Env.IGNORE_PATTERN }}"
  ignore_tag_prefixes:
    - "internal/"
    - "test-"
```

Both `ignore_tags` and `ignore_tag_prefixes` support template rendering, so you can use environment variables or other template expressions.

## Pre-release suffix

When set, influences how prerelease tags are identified for sort ordering:

```yaml
git:
  prerelease_suffix: "-rc"
```

For the legacy `-version:*` modes, setting `prerelease_suffix` forces git-delegated sorting (via `git -c versionsort.suffix=<suffix>`) so the suffix takes effect natively. For `semver` and `smartsemver`, prerelease detection uses the SemVer parser directly: any tag with a `-`-separated prerelease component (e.g. `v1.2.3-rc.1`, `v1.2.3-beta`, `v1.2.3-rc1`) is classified as a prerelease.

## Detected git info

Anodizer detects the following information from git, all available as template variables:

| Variable | Description |
|----------|-------------|
| `{{ Tag }}` | Current git tag |
| `{{ Commit }}` | Full commit SHA |
| `{{ ShortCommit }}` | Short commit SHA |
| `{{ Branch }}` | Current branch name |
| `{{ CommitDate }}` | Commit date (ISO 8601) |
| `{{ CommitTimestamp }}` | Commit timestamp (Unix) |
| `{{ PreviousTag }}` | Previous git tag |
| `{{ Summary }}` | `git describe` output |
| `{{ TagSubject }}` | Tag annotation subject |
| `{{ TagBody }}` | Tag annotation body |
| `{{ TagContents }}` | Full tag annotation |
| `{{ IsSnapshot }}` | Whether this is a snapshot build |
| `{{ IsGitDirty }}` | Whether the working tree has uncommitted changes |
| `{{ IsGitClean }}` | Inverse of `IsGitDirty` |
| `{{ GitTreeState }}` | `"clean"` or `"dirty"` |
| `{{ GitURL }}` | Git remote URL (credentials stripped) |
| `{{ Version }}` | Semver version (tag without `v` prefix) |
| `{{ RawVersion }}` | Raw version string before normalization |
| `{{ Major }}` | Semver major version number |
| `{{ Minor }}` | Semver minor version number |
| `{{ Patch }}` | Semver patch version number |
| `{{ Prerelease }}` | Semver pre-release suffix (e.g., `rc1`) |

## Full example

```yaml
git:
  tag_sort: "-version:creatordate"
  ignore_tags:
    - "nightly*"
    - "legacy-*"
  ignore_tag_prefixes:
    - "internal/"
    - "test-"
  prerelease_suffix: "-rc"
```
