+++
title = "Before/After Hooks"
description = "Run commands before or after the release pipeline"
weight = 89
template = "docs.html"
+++

Anodizer supports running arbitrary shell commands before the pipeline starts and after it completes. These are the same hooks documented in [Global Hooks](/docs/general/hooks/), but this page focuses on their use around the publish phase.

## Classification

Not applicable — this is a workflow page, not a publisher. Hooks run arbitrary user commands; classification depends on what those commands do.

## Minimal config

```yaml
before:
  hooks:
    - "echo 'Starting release'"

after:
  hooks:
    - "echo 'Release complete'"
```

## Full config reference

```yaml
before:
  hooks:
    - "cargo test --release"                       # shorthand: bare command string
    - cmd: "cargo build --release"                 # structured form
      dir: "{{ .Env.PROJECT_ROOT }}"
      env:
        RUST_LOG: info
      output: true

after:
  hooks:
    - cmd: "./scripts/deploy.sh"
      env:
        DEPLOY_TARGET: production
```

### Structured hook fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `cmd` | string | **required** | Command to execute. Rendered through the template engine before execution. |
| `dir` | string | project root | Working directory for the command. Template-rendered. |
| `env` | map | none | Additional environment variables (merged with inherited env). |
| `output` | bool | `false` | Capture and log stdout/stderr (with secret redaction). |

## Authentication

Not applicable — this is a workflow page, not a publisher. Any credentials your hook commands need must be present in the environment at hook runtime.

## Common gotchas

- Hook commands are rendered through the template engine before execution. Escape literal `{{` braces if needed.
- The process environment is inherited; pipeline environment variables (`VERSION`, `TAG`, etc.) are available.
- Secrets are automatically redacted from stdout/stderr.
- `hooks` is accepted as an alias for `pre` (GoReleaser compatibility).
- Before hooks run sequentially; a failing hook aborts the pipeline.
- After hooks only run if all stages complete successfully — they are not a cleanup mechanism for failed runs.

## Use cases

- **Pre-flight checks**: `cargo fmt --check`, `cargo clippy`, `cargo test`
- **Post-release notifications**: Slack webhooks, deployment triggers
- **Artifact post-processing**: signing, uploading to additional locations
- **Environment setup**: setting up credentials or config before publish
