+++
title = "Global Hooks"
description = "Run shell commands before or after the release pipeline"
weight = 4
template = "docs.html"
+++

Hooks let you run arbitrary shell commands at the start or end of the release pipeline.

## Minimal config

```yaml
before:
  hooks:
    - "echo 'Starting release'"
    - "cargo fmt --check"

after:
  hooks:
    - "echo 'Release complete'"
    - "./scripts/notify.sh"
```

## Behavior

- **`before` hooks** run before any pipeline stage executes
- **`after` hooks** run after all pipeline stages complete successfully
- Each hook is executed via `sh -c "<command>"`
- If any `before` hook fails (non-zero exit), the pipeline aborts
- Hooks are skipped in `--dry-run` mode (logged but not executed)
- Environment variables from the `env` config section are available to hooks

## Use cases

- Pre-flight checks: `cargo fmt --check`, `cargo clippy`
- Post-release notifications: Slack webhooks, deployment triggers
- Artifact post-processing: signing, uploading to additional locations
