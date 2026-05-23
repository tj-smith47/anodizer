+++
title = "Snapshots"
description = "Build locally without publishing"
weight = 9
template = "docs.html"
+++

Snapshot mode runs the full build and archive pipeline but skips all publishing stages.

## Classification

Not applicable — this is a workflow page, not a publisher. Snapshot mode disables every publisher in the pipeline (including Submitters); the only outputs are local artifacts under `dist/`.

## Minimal config

```bash
anodizer release --snapshot
```

No YAML changes required for the default behavior.

## Full config reference

```yaml
snapshot:
  version_template: "{{ .Version }}-SNAPSHOT-{{ .ShortCommit }}"  # optional; version suffix (alias: name_template)
```

## Authentication

Not applicable — snapshot mode never contacts external services. No tokens are read or required.

## Common gotchas

- The default template appends `-SNAPSHOT` to the version; override via `snapshot.version_template` (or its deprecated alias `name_template`).
- `--auto-snapshot` engages snapshot mode whenever the git repo has uncommitted changes — useful for safety in CI.
- Required publishers are silently skipped; snapshots never publish regardless of the `required` flag.

## Auto-snapshot

Automatically enable snapshot mode when the git repo has uncommitted changes:

```bash
anodizer release --auto-snapshot
```
