+++
title = "Custom Publishers"
description = "Run arbitrary commands on release artifacts"
weight = 11
template = "docs.html"
+++

Custom publishers let you run any command on your release artifacts, enabling integration with tools and registries that anodizer doesn't natively support.

## Classification

Depends on the command — anodizer cannot classify an arbitrary subprocess. If your command pushes to a remote service, the publisher is Assets, Manager, or Submitter depending on rollback behavior. Treat it as Submitter (no rollback) by default unless your `cmd` is provably idempotent and reversible.

## Minimal config

```yaml
publishers:
  - name: my-publisher
    cmd: ./scripts/publish.sh
    args: ["{{ ArtifactPath }}", "{{ Version }}"]
```

## Full config reference

```yaml
publishers:
  - name: my-publisher              # required; logging identifier
    cmd: ./scripts/publish.sh       # required; command to execute
    args: []                        # optional; arguments (templates supported)
    ids: []                         # optional; only run on artifacts with these IDs
    artifact_types: []              # optional; binary | archive | checksum | package
    env: {}                         # optional; extra environment variables
```

## Authentication

Not applicable to anodizer — credentials are the responsibility of the command you invoke. Anodizer's `user_command` constructor whitelists the env passed to subprocesses to prevent unintended credential leakage; you must explicitly pass any required env vars via the `env:` field.

## Common gotchas

- Custom publishers run sequentially in publisher order. Slow commands block the pipeline.
- `args` entries are template-rendered individually; quote them in YAML if they contain spaces.
- The env whitelist means most ambient credentials are NOT visible to the subprocess — declare every var you need via `env:`.
- `artifact_types` filters at dispatch time; if no artifacts match, the publisher is a no-op (no error).

## Filtering artifacts

```yaml
publishers:
  - name: upload-binaries
    cmd: ./scripts/upload.sh
    artifact_types: [binary]
    args: ["{{ ArtifactPath }}"]
```
