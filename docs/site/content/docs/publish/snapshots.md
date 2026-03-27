+++
title = "Snapshots"
description = "Build locally without publishing"
weight = 9
template = "docs.html"
+++

Snapshot mode runs the full build and archive pipeline but skips all publishing stages.

## Usage

```bash
anodize release --snapshot
```

## Config

Customize the snapshot version suffix:

```yaml
snapshot:
  name_template: "{{ Version }}-SNAPSHOT-{{ ShortCommit }}"
```

The default template appends `-SNAPSHOT` to the version.

## Auto-snapshot

Automatically enable snapshot mode when the git repo has uncommitted changes:

```bash
anodize release --auto-snapshot
```
