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
- **`before_publish` hooks** run after build / archive / sign / sbom /
  checksum complete but before any publisher dispatches. They fire **once
  per matching artifact** by default (with `{{ ArtifactName }}` /
  `$ANODIZER_ARTIFACT` bound), or **once** with run-level vars when
  `run_once: true` — see [Before-Publish Hooks](/docs/publish/before-publish/)
  for the full reference
- Each hook is executed via `sh -c "<command>"`
- If any `before` or `before_publish` hook fails (non-zero exit), the
  pipeline aborts before any subsequent stage runs
- Hooks are skipped in `--dry-run` mode (logged but not executed)
- Environment variables from the `env` config section are available to hooks

## Back-compat alias: `post:`

Older anodizer configs use `after.post:` instead of `after.hooks:`. The
old spelling is still accepted (folded into `hooks:` at parse time with
a deprecation warning) so existing configs keep working, but new
configs should match GoReleaser Pro and use `hooks:` for both `before:`
and `after:` blocks.

## Use cases

- Pre-flight checks: `cargo fmt --check`, `cargo clippy`
- Post-release notifications: Slack webhooks, deployment triggers
- Artifact post-processing: signing, uploading to additional locations
