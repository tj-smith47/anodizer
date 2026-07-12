+++
title = "Before-Publish Hooks"
description = "Run validators against the staged dist tree immediately before publishers fire"
weight = 90
template = "docs.html"
+++

Anodizer supports three lifecycle hook blocks at the top level of the config:

- `before:` — runs once at pipeline start, before any build.
- `after:` — runs once at the end, only if every stage succeeded.
- `before_publish:` — runs after build, archive, sign, sbom, and checksum
  complete, but **immediately before** any publisher dispatches.

This page documents `before_publish:`. For the broader lifecycle hooks see
[Global Hooks](/docs/general/hooks/).

## When `before_publish:` fires

```
build → archive → sbom → checksum → sign → [ before_publish ] → release → blob → publish → announce
```

The hook is the last gate before any publisher writes to a registry. A
non-zero exit from any hook aborts the release before `release` /
`publish` / `blob` / `snapcraft-publish` / `announce` run.

Use cases:

- Run a smoke test against the staged dist tree (e.g. unpack the archive
  and exercise the binary).
- Run a vulnerability scanner / antivirus against final artifacts.
- Stage external state (push a placeholder commit, reserve a registry
  slot, page on-call).
- Abort the release based on a custom invariant the pipeline can't express
  (e.g. "no artifact may exceed 50 MiB").

## Minimal config

```yaml
before_publish:
  hooks:
    - "./scripts/smoke-test.sh"
```

## Full config reference

```yaml
before_publish:
  hooks:
    # Shorthand: bare command string.
    - "./scripts/smoke-test.sh"

    # Structured form with all fields.
    - cmd: "./scripts/scan-artifacts.sh {{ Tag }}"
      dir: "./scripts"
      env:
        - "SCAN_PROFILE=release"
        - "SLACK_WEBHOOK={{ Env.SLACK_WEBHOOK }}"
      output: true
      if: "{{ not IsSnapshot }}"
```

### Structured hook fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `cmd` | string | **required** | Command to execute under `sh -c`. Rendered through the template engine before execution. |
| `dir` | string | project root | Working directory for the command. Template-rendered. |
| `env` | list of `KEY=VALUE` | none | Additional environment variables for the hook. Template-rendered. The host environment is also inherited; per-hook values override inherited keys of the same name. |
| `output` | bool | `false` | When `true`, stream stdout/stderr to anodizer's logger in real time. When `false`, output is captured and only surfaced if the hook fails (with secrets redacted). |
| `if` | string template | unset | When set, the hook only runs if the rendered result is truthy (not `"false"` / `"0"` / `"no"` / empty). Render failure hard-errors. Same surface as build / archive / sign hooks' `if:`. |
| `ids` | list of strings | none | Artifact-id allow-list. When set, the per-artifact iteration only fires for artifacts whose `id` matches one of these. Ignored when `run_once: true`. |
| `artifacts` | enum | `all` | Artifact-kind filter (`checksum` / `source` / `package` / `installer` / `diskimage` / `archive` / `binary` / `sbom` / `image` / `all`). The hook fires only for matching artifacts. Ignored when `run_once: true`. |
| `run_once` | bool | `false` | Run the hook a single time with run-level vars instead of once per matching artifact. See [Per-artifact iteration and `run_once`](#per-artifact-iteration-and-run-once). |

### Execution order

Hooks fire **sequentially**, in declared order. A failure short-circuits
the remaining hooks AND the publish phase.

## Per-artifact iteration and `run_once`

By default a `before_publish` hook runs **once per matching artifact**. anodizer
iterates every artifact in the staged `dist/` tree (narrowed by the hook's `ids`
/ `artifacts` filters) and runs the command once for each, with the per-artifact
template variables and the `$ANODIZER_ARTIFACT` environment channel bound:

| Variable | Env channel | Value |
|----------|-------------|-------|
| `{{ ArtifactName }}` | `$ANODIZER_ARTIFACT` | Artifact filename (e.g. `myapp-1.0.0-linux-amd64.tar.gz`) |
| `{{ ArtifactPath }}` | — | Full path to the artifact under `dist/` |
| `{{ ArtifactExt }}` | — | Compound-aware extension (`.tar.gz`, `.deb`) |
| `{{ ArtifactKind }}` | — | Artifact kind (`archive`, `package`, `binary`, …) |
| `{{ ArtifactID }}` | — | The artifact's configured `id` |
| `{{ Os }}` / `{{ Arch }}` / `{{ Target }}` | — | The artifact's platform triple, split |

```yaml
before_publish:
  hooks:
    # Runs once per package artifact, scanning each one by name.
    - cmd: "clamscan {{ ArtifactPath }}"
      artifacts: package
```

Set **`run_once: true`** to run the command a **single time** with the run-level
template vars (`{{ .Version }}`, `{{ .Tag }}`, …) instead. This is a
Rust-additive extension with no GoReleaser counterpart — it suits a one-shot
upload, a single notification, or any step that should fire once regardless of
how many artifacts the build produced:

```yaml
before_publish:
  hooks:
    # Runs exactly once, whatever the artifact count.
    - cmd: "./scripts/notify-staging.sh {{ Version }}"
      run_once: true
```

When `run_once: true`:

- The `ids` and `artifacts` filters **do not apply** — they are per-artifact
  concepts.
- The per-artifact vars (`{{ ArtifactName }}`, `{{ ArtifactPath }}`, …) and
  `$ANODIZER_ARTIFACT` are **not** bound.
- If the command needs the artifacts, it must **iterate `dist/` itself** (e.g.
  `for f in dist/*.tar.gz; do …; done`).

A non-zero exit still aborts the release, exactly as in the per-artifact case.

## Skipping the hook

Use `--skip=before-publish` on the `anodizer release` command to bypass the
entire block (e.g. during a hotfix where the validators would block a
critical patch):

```bash
anodizer release --skip=before-publish
```

The stage is also skipped automatically in dry-run mode for any hook
whose `cmd` would be destructive — under `--dry-run` the rendered command
is logged but the subprocess is not spawned.

## Common gotchas

- The hook receives the **staged** dist tree under `./dist/`, not the
  published one. If your validator depends on download URLs, use `before:`
  (pre-build) or `after:` (post-publish) instead.
- `env:` values are **rendered** through the template engine before
  injection, so `{{ Tag }}` and `{{ Env.VAR }}` expand inside per-hook
  env strings.
- Hooks inherit the host environment by default. Secret values are
  automatically redacted from any captured stdout/stderr by anodizer's
  secret-redaction filter before any log emission.
- Sequential execution: a scanner hook may safely depend on artifacts
  produced by an earlier hook in the same block.

## Authentication

Not applicable — this block runs arbitrary user commands. Any credentials
your hook commands need must be present in the environment at hook
runtime (e.g. via `env_files:` or CI secrets).
