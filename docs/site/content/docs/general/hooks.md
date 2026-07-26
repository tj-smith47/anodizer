+++
title = "Global Hooks"
description = "Run shell commands before, after, or either way around the release pipeline"
weight = 4
template = "docs.html"
+++

Hooks let you run arbitrary shell commands around the release pipeline — before
it starts, after it succeeds, after it fails, or on every path either way.

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

on_error:
  hooks:
    - "./scripts/notify-release-failed.sh"

always:
  hooks:
    - "./scripts/teardown-staging.sh"
```

## The lanes

| Block | Runs when | Ordering | `--skip` token |
|---|---|---|---|
| `before` | before any pipeline stage | first | `before` |
| `before_publish` | after the artifacts are built, before any publisher | mid-pipeline | `before-publish` |
| `after` | the pipeline finished **successfully** | after the pipeline | `after` |
| `on_error` | the pipeline **failed** at any stage | after the failure | `on-error` |
| `always` | **every** terminal path, success or failure | **last**, after `after` / `on_error` | `always` |

The four outer lanes map onto `try` / `else` / `catch` / `finally`:

```yaml
before:   { hooks: ["./stage-secrets.sh"] }    # try
after:    { hooks: ["./notify.sh ok"] }        # success only
on_error: { hooks: ["./notify.sh failed"] }    # failure only
always:   { hooks: ["./teardown-staging.sh"] } # finally — both, runs last
```

## Which commands run which lanes

| Lane | `anodizer release` | `anodizer build` | `anodizer publish` |
|---|---|---|---|
| `before` | ✅ | ✅ | — |
| `before_publish` | ✅ | — | ✅ |
| `after` | ✅ | ✅ | — |
| `on_error` | ✅ | — | — |
| `always` | ✅ | ✅ | — |

`anodizer build` opens the same bracket `release` does, so state a `before`
hook stages has a teardown lane on the command that staged it:

```
$ anodizer build
  • ran before hook: ./stage-staging.sh
  • built binary myapp (x86_64-unknown-linux-gnu)
  • ran after hook: ./notify.sh built
  • build complete
  • ran always hook: ./teardown-staging.sh
```

`on_error` stays release-only on purpose: it is the release-failed
notification lane, and a local build failure is not a failed release. A
failed `anodizer build` still reaches `always` with `$ANODIZER_SUCCESS=false`,
which is where build teardown belongs.

Every other command (`publish`, `announce`, `check`, `tag`, ...) runs no root
lane at all — `publish` runs only the mid-pipeline `before_publish` hooks.

## Suppressing a lane with `--skip`

Every root lane has a `--skip` token, and `release` and `build` accept the
same four, so one skip list works on whichever command a job runs:

```bash
anodizer release --skip=on-error       # ship without the failure notifier
anodizer build   --skip=before,always  # no staging, no teardown
```

The token is the block name in kebab-case, so `on_error:` is `--skip=on-error`
(the same shape `before_publish:` → `--skip=before-publish` uses). An
unrecognized token is a hard usage error listing the valid set — a `--skip`
value is never accepted and silently ignored:

```
$ anodizer build --skip=post-hooks
Error: invalid --skip value(s): post-hooks. Valid options: before, after,
always, on-error, validate, sign, notarize
```

One token covers every scope the lane fires in: `--skip=before` suppresses
the root `before:` block AND every `crates[].before:` block, and
`--skip=on-error` suppresses the root `on_error:` block AND every
`publish.on_error:` block.

`--skip=always` is deliberate, not an oversight. `always` is the run's
`finally`, but `--skip=before` already suppresses the lane it pairs with, and
a teardown hook firing against state nothing staged is the incoherent half of
that pair. The cost is the obvious one: **teardown does not run**, so
anything the run staged stays staged.

`anodizer build` has no `on_error:` lane, so `build --skip=on-error` is
accepted with nothing to suppress. The token stays in build's vocabulary so a
caller's single skip list does not have to vary by command.

## Behavior

- **`before` hooks** run before any pipeline stage executes
- **`after` hooks** run after all pipeline stages complete successfully.
  A failed run never reaches them
- **`before_publish` hooks** run after build / archive / sign / sbom /
  checksum complete but before any publisher dispatches. They fire **once
  per matching artifact** by default (with `{{ ArtifactName }}` /
  `$ANODIZER_ARTIFACT` bound), or **once** with run-level vars when
  `run_once: true` — see [Before-Publish Hooks](/docs/publish/before-publish/)
  for the full reference
- **`on_error` hooks** run when the pipeline fails at **any** stage
  (build, sign, package, publish, ...). The failure context is exported as
  environment variables — `$ANODIZER_ERROR` (the pipeline error),
  `$ANODIZER_ROLLED_BACK` (always `false`; a release run never withdraws
  anything on its own — withdrawal is `anodizer tag rollback`),
  `$ANODIZER_VERSION`, `$ANODIZER_TAG` — and as template vars
  (`{{ .Error }}`, `{{ .RolledBack }}`). Read the error via the env var,
  not template interpolation, to stay shell-injection-safe. An `on_error`
  hook's own failure is logged and never masks the pipeline error
- **`always` hooks** run **last on every terminal path** — after `after` on
  a successful run, after `on_error` on a failed one, and also when a
  `before` hook failed before the pipeline ever started, which is the one
  exit neither of the other two reaches. Use them for teardown that has to
  happen either way: removing a staging directory, releasing a lock,
  stopping a sidecar container. The outcome is exported as
  `$ANODIZER_SUCCESS` (`true` / `false`), `$ANODIZER_ERROR` (empty on
  success), `$ANODIZER_VERSION`, `$ANODIZER_TAG` — and as template vars
  (`{{ .Success }}`, `{{ .Error }}`). `{{ .Success }}` is a real boolean, so
  `{% if Success %}` branches correctly
- Each hook is executed via `sh -c "<command>"`
- If any `before` or `before_publish` hook fails (non-zero exit), the
  pipeline aborts before any subsequent stage runs
- Hooks are skipped in `--dry-run` mode (logged but not executed)
- Environment variables from the `env` config section are available to hooks

## `always` and multi-host releases

`always` fires **once per invocation**, pairing 1:1 with `before`. A split
fan-out is several invocations, so both blocks run on each of them:

```
release --split   (shard 1)   before → build            → always
release --split   (shard 2)   before → build            → always
release --merge               before → post-build → after → always
```

`after` is the exception: it fires only on the merge, because the shard leg
stops at the build stage and never reaches the post-pipeline tail. That is
exactly why teardown belongs in `always` — a shard that staged something in
`before` gets to clean it up.

## Root hooks in a workspace

The four root blocks are one **global** lane. Each fires **once per
invocation** — never once per crate — including a per-crate publish
(`anodizer release --publish-only` over a `dist/<crate>/` tree), where the
root `after` block fires once after the last crate rather than once per
crate.

Per-crate hooks are a separate surface with per-crate cardinality:

```yaml
after:                              # once per run, after the last crate
  hooks: ["./notify.sh released"]

crates:
  - name: core
    after:                          # once per crate, inside core's scope
      hooks: ["./post-core.sh"]
  - name: cli
    after:
      hooks: ["./post-cli.sh"]
```

```
release --publish-only   before → [core → post-core.sh] → [cli → post-cli.sh]
                                → notify.sh released → always
```

A per-crate block renders against that crate's own `{{ Version }}` /
`{{ Tag }}`; a root block renders against the run's.

## A failing `always` hook

| Run outcome | A failing `always` hook does |
|---|---|
| failed | log a warning; the run still exits with the **original** pipeline error |
| succeeded | fail the run (nothing to mask — same contract as `after`) |

The operator always sees what actually broke the release; a broken teardown
script can never overwrite that diagnosis.

## Back-compat alias: `post:`

Older anodizer configs use `after.post:` instead of `after.hooks:`. The
old spelling is still accepted (folded into `hooks:` at parse time with
a deprecation warning) so existing configs keep working, but new
configs should match GoReleaser Pro and use `hooks:` in every block.

## Use cases

- Pre-flight checks: `cargo fmt --check`, `cargo clippy`
- Post-release notifications: Slack webhooks, deployment triggers
- Artifact post-processing: signing, uploading to additional locations
