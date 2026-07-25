+++
title = "Release Resilience"
description = "Three-group publisher dispatch, the Submitter gate, and convergent recovery via re-run"
weight = 6
template = "docs.html"
+++

Releases fan out to many publishers (GitHub Releases, crates.io, npm, PyPI,
Homebrew taps, Docker Hub, Cloudsmith, Artifactory, Gem Fury, Scoop, Nix,
Krew, SchemaStore, MCP, AUR, Snapcraft, Chocolatey, Winget, blob storage).
Each has a different cost of failure. A botched Docker Hub description sync is
a no-op for end users; a botched `cargo publish` burns a version slot forever.
Anodizer's release pipeline is shaped around that asymmetry.

This guide walks through:

- The three-line recovery model — what to type when a release fails.
- The three publisher groups (Assets / Manager / Submitter) and why dispatch order matters.
- The Submitter gate that prevents irreversible publishers from firing after a required failure.
- `reconcile()` — the primitive that makes a re-run converge instead of double-publishing.
- `release.on_failure: hold` — what a failed run leaves behind, and why there is no automatic rollback.
- `--fail-fast` and how it differs from the default collect-then-decide behavior.
- `anodizer tag rollback` — deliberate withdrawal of a release, including its publisher-unwind step.
- `--summary-json=<path>` for capturing the audit trail.

## The recovery model

Start here. Three lines cover every failed release; everything below this
section explains why each one is safe.

| Situation | What you type | Why it works |
|---|---|---|
| A release failed partway | the **identical** `anodizer release` command — same tag, same flags | publishers that already landed this exact version `reconcile()` to `Complete` and skip themselves; the failed ones retry |
| This version should not exist at all | `anodizer tag rollback` | deletes the anodize-managed tag(s), reverts the bump commit, and unwinds every publisher recorded `Succeeded` |
| A publisher reports `Diverged` | bump the version, then release again | the version is already published upstream with *different* bytes, and that registry slot is immutable — no re-run and no rollback can overwrite it |

> **Re-running is for a failed PUBLISHER; `anodizer continue` is for a failed
> STAGE.** A re-run reconciles each publisher against its upstream and
> self-skips what already landed. `continue` resumes a pipeline that stalled
> before publishing — it skips the stages that already completed rather than
> the publishers that already published. See
> [`publish` vs `continue`](@/docs/general/release-workflow.md#publish-vs-continue).

A worked convergence, with homebrew marked `required: true` so the failure
closes the Submitter gate before cargo can fire:

```text
$ anodizer release
   • created GitHub Release 'v0.2.1' (id=178342119) on acme/widget
   • published release 'v0.2.1' (draft → live)
   • skipping cargo — gated by an earlier required failure (one-way-door protection)
   • wrote run-report to dist/run-v0.2.1/report.json
Error: 1 required publisher(s) failed: homebrew. The release pipeline ran to
completion, so rollback / announce-gating / summary all observed final state;
this non-zero exit ensures CI and shell callers see the failure. Inspect
dist/run-<id>/report.json for details; re-run the same release command to
converge, or `anodizer tag rollback` to withdraw the release deliberately.

# fix the branch-protection rule, then run the EXACT SAME command:
$ anodizer release
   • release 'v0.2.1' already live (id=178342119, mode=keep-existing)
   • Homebrew tap acme/homebrew-tap updated for 'widget'
   • published crate 'widget-core'
```

Dispatch order is what makes the second run correct: the reversible groups go
first, so a Manager failure gates the one-way doors *before* they fire (see
[Publisher groups](#publisher-groups)).

There is no `--rollback` flag, no automatic in-process rollback, and no
intermediate "unwind the reversible publishers, then cut a fresh tag" step.
`anodizer tag --push` never has to run just to get a clean slate — the failed
tag stays exactly where it is, and the SAME tag's release converges on re-run.

> **The one state a re-run cannot fix is `Diverged`.** The version is burned
> upstream with content that does not match what you are about to ship, so a
> re-run would be silently skipped by the registry. Bump the version. See
> [Convergent re-run](#convergent-re-run) for the full state table and
> [`tag rollback`](#recovering-a-poisoned-tag-with-tag-rollback) for the flag
> matrix and the published-state guard.

## Release-stage retry flags

Two flags on the `release:` block make individual release-stage runs idempotent
without requiring a full rollback:

- `release.replace_existing_draft` — DELETE-and-recreate a draft release with the same name
- `release.replace_existing_artifacts` — DELETE-and-re-upload an asset that conflicts with new bytes

Both are safe to set permanently; they are no-ops when there is no existing draft or conflicting asset.
See [Recovery flags](./recovery-flags.md) for the full mechanism, the equivalent flags on every other
publisher, and operational guidance.

Independently of both flags, a re-run that would upload byte-identical assets is a no-op on every
forge (GitHub, GitLab, Gitea) — the flags only govern *differing* bytes and stale drafts.

## Publisher groups

Every publisher is classified into exactly one group, based on how recoverable
a failure is:

| Group | Property | Examples |
|---|---|---|
| Assets | Writes uploadable bytes to systems anodizer controls end-to-end. Reversible via API delete. | github-release, dockerhub, artifactory, uploads, cloudsmith, blob |
| Manager | Writes to package-manager state. Server-side deletable, but consumer machines may already have pulled the artifact. | homebrew, scoop, nix, aur, krew, mcp, schemastore, gemfury |
| Submitter | Writes to a third-party submission queue, an immutable registry slot, or a channel position that cannot be reclaimed. | cargo, npm, pypi, homebrew-core, chocolatey, winget, snapcraft, upstream-aur |

Within `PublishStage`, dispatch order is Assets, then Manager, then Submitter.
Order inside a group matches the existing (per-publisher) dispatch order.
Snapcraft stays in its own stage running after `PublishStage`; it is Submitter
group and has no rollback, so the existing stage boundary is fine.

Blob runs as its own stage BEFORE `PublishStage` (and `SnapcraftPublishStage`)
so that a required-blob upload failure is recorded in the publish report before
the Submitter gate evaluates — gating the one-way-door publishers
(cargo / chocolatey / winget) as well as Snapcraft via the same gate logic.
Ordered after `PublishStage`, a blob failure could only ever gate the
still-later Snapcraft stage while cargo / chocolatey / winget had already fired
irreversibly. Blob needs only the built dist, so running it ahead of the doors
is safe.

## Per-publisher classification

The "Rollback action" column below is invoked only by `anodizer tag rollback`
— never automatically. A release run that fails leaves published state exactly
where it landed; withdrawing it is always an explicit operator command (see
[Recovering a poisoned tag with `tag rollback`](#recovering-a-poisoned-tag-with-tag-rollback)).

| Publisher | Group | required (default) | Rollback action | Token scope |
|---|---|---|---|---|
| github-release | Assets | **true** | delete the release and its uploaded assets (tag refs untouched — `tag rollback` owns them) | `GITHUB_TOKEN contents:write` |
| dockerhub | Assets | false | PATCH the repo description back to the pre-publish snapshot | `DOCKER_PASSWORD description snapshot+restore` |
| artifactory | Assets | false | parallel HTTP DELETE per uploaded URL (404/410 treated as already-absent) | `ARTIFACTORY_TOKEN delete` |
| uploads | Assets | false | HTTP DELETE per recorded upload URL | `UPLOAD_<NAME>_SECRET delete` |
| cloudsmith | Assets | false | DELETE `/packages/<org>/<repo>/<slug>/`; warn-only manual checklist when the API key is absent | `CLOUDSMITH_API_KEY package_delete` |
| blob (s3/gcs/azure) | Assets (own stage) | false | delete each object actually written | provider creds (`AWS_*` / `GOOGLE_APPLICATION_CREDENTIALS` / `AZURE_STORAGE_*`); no single env gate |
| homebrew (tap + casks) | Manager | false | re-clone, `git revert HEAD --no-edit`, push | `GITHUB_TOKEN contents:write` |
| scoop (bucket) | Manager | false | re-clone, `git revert HEAD --no-edit`, push | `GITHUB_TOKEN contents:write` |
| nix (overlay repo) | Manager | false | re-clone, `git revert HEAD --no-edit`, push | `GITHUB_TOKEN contents:write` |
| aur (your own AUR repos) | Manager | false | re-clone, `git revert HEAD --no-edit`, push | `AUR_SSH_KEY write` |
| krew | Manager | false | PrDirect: close the PR anodizer opened. BotWebhook: no-op (the krew-release-bot server owns the krew-index PR) | `GITHUB_TOKEN pull_request:write` (PrDirect) |
| mcp | Manager | false | PATCH the server's registry status; degrades to a warn when the registry rejects it | `MCP_GITHUB_TOKEN status-mutation` |
| schemastore | Manager | false | close the SchemaStore PR anodizer opened | `GITHUB_TOKEN pull_request:write` |
| gemfury | Manager | **true** | DELETE each pushed package version | `FURY_API_TOKEN delete` |
| cargo | Submitter | **true** | `cargo yank` per crate this run published (the version slot stays reserved) | `CARGO_REGISTRY_TOKEN yank` |
| npm | Submitter | **true** | `npm unpublish` per published target; outside npm's unpublish window it warns for manual cleanup | `NPM_TOKEN unpublish` |
| pypi | Submitter | **true** | none — a PyPI filename can never be re-uploaded, so the unwind warns per uploaded file | n/a |
| homebrew-core | Submitter | false | close the bump PR; a direct-commit bump warns for a manual revert | `GITHUB_TOKEN pull_request:write` |
| chocolatey | Submitter | false | none — warn per package with its gallery URL (no programmatic withdraw endpoint) | n/a |
| winget | Submitter | false | none — warn per target with the fork-branch query (upstream validation cannot be cancelled mid-flight) | `GITHUB_TOKEN pull_request:write` (preflight bookkeeping; warn-only at runtime) |
| upstream-aur (force-push) | Submitter | false | none — warn per recorded force-push | `AUR_SSH_KEY write` |
| snapcraft | Submitter (own stage) | derived from config | none — snapcraft registers no `rollback()`, and already-installed snaps keep the revision | n/a |

Custom `publishers:` entries are not in this table: they run after the
pipeline stages rather than inside group dispatch, so they carry no group, no
`required` gate, and no unwind.

`required: true` means the release pipeline treats this publisher's failure as
fatal for downstream gating. The defaults reflect operator intent — the
registries a consumer installs from must succeed for a release to mean
anything; everything else is opportunistic:

| Default `required: true` | Default `required: false` |
|---|---|
| github-release, cargo, npm, pypi, gemfury | every other publisher |

Override per-publisher in your config:

```yaml
publish:
  homebrew_cask:
    required: true     # block submitter dispatch + announce on tap failure
```

## The Submitter gate

Before dispatching each Manager- and Submitter-group publisher, anodizer
re-inspects the in-progress `PublishReport`:

- Once any `required: true` publisher has failed, every remaining Manager and
  Submitter publisher is skipped and recorded as `skipped-submitter-gated`.
  Re-checking per publisher (rather than once per group) is what makes the
  intra-group ordering safe — each remaining one-way door consults live state.
- While every `required: true` publisher has succeeded, dispatch proceeds
  normally even though `required: false` publishers have failed.

Each gated publisher prints one line:

```text
   • skipping cargo — gated by an earlier required failure (one-way-door protection)
```

The gate is on by default. Operator opt-out:

```bash
anodizer release --no-gate-submitter
```

Use this only when you have manually verified the failed publisher is not
load-bearing for the release. The default keeps you from burning a crates.io
version slot because a homebrew tap push happened to hit a branch-protection
glitch.

## Convergent re-run

`reconcile()` is a cheap, read-only answer to "am I already done for this
exact version?" that the publish dispatch loop consults before calling a
publisher's `run()`.

| | Publishers | How a re-run stays safe |
|---|---|---|
| Implement `reconcile()` | cargo, npm, pypi, homebrew, homebrew-core, scoop, nix, krew, winget, chocolatey | a probe of the upstream registry / open PR returns `Complete`, and `run()` is skipped entirely |
| Inherit the default `Absent` | everything else | `run()` dispatches and is idempotent on its own terms — github-release PATCHes the release it already created, artifactory and blob skip a byte-identical object already at the path, cloudsmith skips a file whose md5 already matches |

A `Complete` verdict prints one line and the publisher never runs:

```text
$ anodizer release
   • skipping cargo — already published for this version (all 3 planned crate(s) already on crates.io with verified content)
```

`reconcile()` returns one of four states, and the dispatch loop reacts
differently to each:

| State | Meaning | Dispatch effect |
|---|---|---|
| `Absent` | Not present upstream yet. | `run()` publishes as normal. |
| `Complete` | This exact version is already published/submitted upstream. | `run()` is skipped; recorded `skipped-already-published`. |
| `Diverged` | The version exists upstream but with **different** local bytes. | Recorded `Failed`; dispatch continues unless `--fail-fast`. A `required: true` publisher's divergence closes the Submitter gate and fails the run at the end-of-pipeline exit-code gate; `required: false` is a tolerated, gate-neutral failure. Either way, this publisher cannot be re-published as-is. |
| `Unknown` | The probe could not determine state (network error, unparseable response). | Never blocks — `run()` proceeds and the registry's own conflict handling is the backstop. Fail-safe toward publishing, never toward silently skipping. |

`Diverged` is the one state a re-run cannot fix by itself: the version is
burned upstream with content that does not match what you are trying to ship.
**Bump the version and release again.** How it is reported depends only on
`required`; dispatch keeps going either way (unless `--fail-fast`), and a
required divergence closes the Submitter gate and makes the final exit
nonzero:

| `required` | Register | Message |
|---|---|---|
| `true` | Error | `cargo: this version is already published upstream with DIFFERENT content — bump the version and re-release. <detail>` |
| `false` | Warning | `npm: version already published with different content — cannot republish (optional publisher, continuing): <detail>` |

The publisher that owns the content check reports the divergence with its own
byte-level detail:

```bash
$ anodizer release
Error: publish: 'anodizer-core-0.2.1' is ALREADY published on crates.io with
DIFFERENT content (index cksum ab12…, local .crate cksum cd34…). Re-publishing
would be SILENTLY SKIPPED by cargo, so the changed code would never ship under
this version. Differing entries: src/lib.rs. Bump the version (crates.io
versions are immutable) and re-run.
```

## `on_error` hooks

Shell hooks that fire once per FAILED publisher, immediately — there is no
automatic rollback step to wait for:

```yaml
publish:
  on_error:
    - cmd: 'anodizer notify --raw "anodizer: $ANODIZER_PUBLISHER failed @ $ANODIZER_VERSION: $ANODIZER_ERROR"'
```

`--raw` sends the message literally, skipping Tera rendering — recommended
here because `$ANODIZER_ERROR` is untrusted (see the security note below).

The failure context is available on two channels — environment variables on
the hook process, and template variables rendered into `cmd`:

| Env var | Template variable | Value |
|---|---|---|
| `ANODIZER_PUBLISHER` | `{{ .Publisher }}` | Publisher name (e.g. `homebrew`) |
| `ANODIZER_ERROR` | `{{ .Error }}` | Error message string |
| `ANODIZER_VERSION` | `{{ .Version }}` | Release version (e.g. `0.8.0`) |
| `ANODIZER_TAG` | `{{ .Tag }}` | Release tag (e.g. `v0.8.0`) |
| `ANODIZER_GROUP` | `{{ .Group }}` | Publisher group: `Assets`, `Manager`, or `Submitter` |
| `ANODIZER_REQUIRED` | `{{ .Required }}` | `true` / `false` |
| `ANODIZER_ROLLED_BACK` | `{{ .RolledBack }}` | Always `false` — a release run never withdraws anything on its own; withdrawal is `anodizer tag rollback` |
| `ANODIZER_RUN_REPORT` | `{{ .RunReport }}` | Path of this run's already-written `dist/run-<id>/report.json` (per-publisher outcomes); empty in snapshot/dry-run or when the report could not be persisted |

In workspace per-crate mode both channels carry the per-crate-scoped
`Version` / `Tag` of the crate being published.

**Security — prefer the env vars for untrusted values.** The rendered `cmd`
string is parsed by `sh -c`, and `{{ .Error }}` carries remote-controlled
text (HTTP error bodies, registry responses, git stderr). Interpolating it
into `cmd` lets crafted error content break your quoting and execute as
shell code:

```yaml
# UNSAFE: a single quote in the error body breaks out of the quoting,
# and the `{{ .Error }}` template form splices the untrusted text into
# the `sh -c` cmd string — a shell-injection surface.
- cmd: "anodizer notify 'failed: {{ .Error }}'"

# SAFE: the shell expands $ANODIZER_ERROR at run time; the value is
# never parsed as shell code, and --raw avoids re-rendering text that
# is already final.
- cmd: 'anodizer notify --raw "failed: $ANODIZER_ERROR"'
```

Template interpolation remains fine for values anodizer controls
(`{{ .Publisher }}`, `{{ .Version }}`, `{{ .Tag }}`, ...).

Two reasons to keep using the env form (`$ANODIZER_ERROR`) plus `--raw`
for untrusted text — neither is covered by outbound redaction:

1. **Shell-injection.** The `{{ .Error }}` template form is spliced into
   the `sh -c` cmd string before the shell parses it, so a crafted error
   body can break your quoting and execute. The `$ANODIZER_ERROR` env form
   is expanded by the shell at run time and is never parsed as code.
2. **Double-rendering.** `--raw` skips Tera, so already-final error text
   is not re-rendered.

Secret *values* in the body are a separate concern, and anodizer already
handles them: the outbound notification body is redacted by default
(see [Notification secret redaction](#notification-secret-redaction)), so a
secret can no longer leak into the message even without `--raw`. Prefer the
env form plus `--raw` anyway, for the two reasons above.

Hook failures are logged as warnings and never change the release outcome.
For ad-hoc notifications (outside a release), use `anodizer notify`.

## `on_rollback` hooks

`on_rollback` fires from `anodizer tag rollback`'s publisher-unwind step —
never automatically during `anodizer release`, which does not touch published
state on failure. When an operator runs `tag rollback` to withdraw a release,
it can revert a publisher that **succeeded and never errored** — a pushed
Homebrew tap, an opened PR — because the operator withdrew the whole run, not
because that publisher itself failed. That reverted-but-not-failed publisher
has no `on_error` surface. `on_rollback` is its notification surface: it fires
once per publisher the unwind reverted, including the succeeded-then-reverted
case, and including a revert that itself failed (`{{ .RollbackFailed }}` is
then `true` — the orphaned-artifact escalation signal).

> **`{{ .Reason }}` always renders empty.** The unwind replays a report a
> *prior* process persisted, so the trigger cause is not in scope for it —
> the variable exists for template compatibility and there is no flag that
> supplies one. Alert on `{{ .Publisher }}`, `{{ .Tag }}` and
> `{{ .RollbackFailed }}` instead.

```yaml
publish:
  on_rollback:
    # renders: "reverted homebrew @ v0.2.1 (reason: )" — the reason is empty
    - cmd: 'anodizer notify "reverted {{ .Publisher }} @ {{ .Tag }} (reason: {{ .Reason }})"'
    # what to write instead:
    - cmd: 'anodizer notify --raw "reverted $ANODIZER_PUBLISHER @ $ANODIZER_TAG (rollback_failed=$ANODIZER_ROLLBACK_FAILED)"'
```

`on_rollback` is independent of `on_error`: a publisher that both failed
during the original `release` run and is later rolled back by `tag rollback`
fires **both** hooks, at different times — they answer different questions.

| Env var | Template variable | Value |
|---|---|---|
| `ANODIZER_PUBLISHER` | `{{ .Publisher }}` | Publisher name (e.g. `homebrew`) |
| `ANODIZER_VERSION` | `{{ .Version }}` | Release version (e.g. `0.8.0`) |
| `ANODIZER_TAG` | `{{ .Tag }}` | Release tag (e.g. `v0.8.0`) |
| `ANODIZER_GROUP` | `{{ .Group }}` | Publisher group: `Assets`, `Manager`, or `Submitter` |
| `ANODIZER_REQUIRED` | `{{ .Required }}` | `true` / `false` |
| `ANODIZER_ROLLBACK_FAILED` | `{{ .RollbackFailed }}` | `true` when the revert itself failed (live artifact needing manual cleanup); `false` on a clean revert |
| `ANODIZER_ERROR` | `{{ .Error }}` | This publisher's own revert failure message; empty on a clean revert |
| `ANODIZER_ROLLBACK_REASON` | `{{ .Reason }}` | Always empty here — the unwind replays state a prior process persisted, so the trigger cause is not available to it |

The same security note applies: `{{ .Error }}` carries untrusted git/API text —
read it from `$ANODIZER_ERROR` with `--raw` rather than interpolating it into
`cmd`. Hook failures are logged as warnings and never change the rollback
outcome or abort the remaining unwinds. In workspace per-crate mode both
channels carry the per-crate-scoped `Version` / `Tag`.

### Rollback scope preflight

Each publisher declares a `rollback_scope_needed` label (the "Token scope"
column of the [per-publisher table](#per-publisher-classification)) — the
credential `anodizer tag rollback` will need if this release is ever
withdrawn. Preflight surfaces missing scope as:

- A warning under default settings.
- A blocker under `--strict`.

## `--fail-fast` vs. default

| Mode | Behavior |
|---|---|
| Default | `PublishStage` keeps dispatching publishers after a failure. The Submitter gate re-reads the collected report before each remaining Manager / Submitter publisher and skips it once a required publisher has failed. |
| `--fail-fast` | First publisher failure aborts the stage. Nothing reaches the Submitter gate. Nothing auto-unwinds what already published — re-run to converge, or `anodizer tag rollback` to withdraw. |

Default mode is the right choice for most releases: it maximizes the chance of
ending up with a consistent set of Assets even if one Manager publisher
hiccups. Use `--fail-fast` only when you want loud diagnostics and have a
human ready to retry.

## `release.on_failure`

When a `release` / `release --publish-only` / `release --merge` run fails,
the pipeline never touches published state on its own — there is nothing to
undo in-process, because [convergent re-run](#convergent-re-run) is how
recovery works. `hold` is the only accepted value:

```yaml
release:
  on_failure: hold   # the only accepted value; also the default — the field is optional
```

| Value | Behavior on a pipeline failure |
|---|---|
| `hold` | Leaves tags, commits, and published state exactly where the failed run left them. Exit is still nonzero. Recover by re-running the identical command — publishers converge — or, to withdraw the release deliberately, `anodizer tag rollback`. |

`on_failure: rollback` is a **hard error at config validation**, not a
silent downgrade — automatic rollback was removed entirely:

```bash
$ anodizer check config
Error: release.on_failure: rollback is no longer supported — automatic rollback
was removed — re-running `anodizer release` converges; use `anodizer tag
rollback` for deliberate withdrawal.
```

The fix is a one-line config edit:

```diff
 release:
-  on_failure: rollback
+  on_failure: hold
```

Since `hold` is the only behavior, most configs can drop `on_failure`
entirely and rely on the default.

### Scope

The setting is a root-level `release:` field: in workspace configs
(lockstep or per-crate) the top-level `release.on_failure` governs the whole
run, and setting it in a crate-level `release:` block is a config-load error
(`validate_on_failure_root_only`).

## The run summary (`--summary-json=<path>`)

Every real release (non-snapshot, non-dry-run) writes the audit trail of the
run to `<dist>/run-<id>/summary.json` — including when a stage fails, so a
failed run always leaves machine-readable publish state for recovery tooling
to inspect before anything destructive (like a tag rollback) fires.
`--summary-json=<path>` redirects the document to an explicit path (and is
honored in every mode, including `--snapshot` / `--dry-run`):

```bash
anodizer release --summary-json=dist/run-summary.json
```

Shape:

```json
{
  "schema_version": 2,
  "anodize_version": "0.2.1",
  "tag": "v0.2.1",
  "submitter_gated": false,
  "announce_gated": false,
  "publishers_succeeded": 1,
  "publishers_failed": 1,
  "irreversibly_published": false,
  "results": [
    {
      "name": "github-release",
      "group": "Assets",
      "required": true,
      "outcome": "Succeeded",
      "evidence": { "publisher": "github-release", "primary_ref": "...", "...": "..." }
    },
    {
      "name": "homebrew",
      "group": "Manager",
      "required": false,
      "outcome": { "Failed": "tap push rejected: branch protection" },
      "evidence": null
    }
  ],
  "determinism_allowlist": { "compile_time": [], "runtime": [] }
}
```

CI consumers can diff this between runs to spot regressions in publisher
reliability without parsing log output. `schema_version` is bumped on any
breaking shape change; `#[serde(deny_unknown_fields)]` on the producer side
keeps drift loud. Version 2 removed the `failure_policy` field (there is no
more in-process rollback policy to record — see
[`release.on_failure`](#release-on-failure) above). A reader built against v2
still parses a v1 summary from an older release: the field is optional on
read, simply absent going forward.

`publishers_succeeded` / `publishers_failed` count outcomes that left durable
published state (respectively, a `failed` outcome).
`irreversibly_published` is the recovery verdict: `true` when any
Submitter-group publisher's publish landed. Submitter targets (crates.io, npm,
PyPI, chocolatey, winget, snapcraft, ...) never accept the same version twice, so
once it flips the version is burned — a tag rollback can only orphan the live
release, never enable a clean same-version re-cut. Even a `rolled-back`
Submitter counts: `cargo yank` withdraws the artifact but does not reopen the
version slot. Reversible publishers (release assets, blobs, tap/bucket/index
commits) never set it; their state is deletable and the same version can be
re-cut, so rollback stays available after they succeed.

`anodizer tag rollback` reads `dist/run-*/summary.json` itself and refuses
when the version is burned (override with `--force`):

```bash
$ anodizer tag rollback
Error: refusing to roll back — one-way-door publisher(s) already accepted these version(s):
  v0.8.0: version burned at cargo, chocolatey
Those registries never accept the same version twice, so deleting the tag(s) and reverting the bump cannot lead to a clean same-version re-cut — tags kept to protect the published state.
next step: fix the failure and cut the NEXT version (auto-tag mints it from the next push). To override anyway: `anodizer tag rollback --force`.
```

For workflows that add their own destructive recovery steps anyway, the
anodizer-action exposes the flag as a step output to gate on:

```yaml
# Advanced — custom workflow-level recovery (not needed by default).
# The id: on the release step is what makes steps.release.* resolvable.
- uses: tj-smith47/anodizer-action@v1
  id: release
  with:
    args: release

- name: Custom recovery
  if: always() && (steps.release.outcome == 'failure' || steps.release.outcome == 'cancelled') && steps.release.outputs.irreversibly_published != 'true'
```

## The outcome set

Per-publisher `outcome` in the report uses this fixed set:

```
Succeeded
Skipped(AlreadyPublished | SubmitterGated | NotConfigured | Snapshot | DryRun | ConfigSkipped | VerifyGateBlocked)
Failed(<message>)
RolledBack
RollbackFailed(<message>)
RollbackSkippedNoScope
```

`RolledBack`, `RollbackFailed`, and `RollbackSkippedNoScope` are written only
by `anodizer tag rollback`'s publisher-unwind pass — a `release` run never
produces them itself, since it never rolls anything back automatically. See
[Recovering a poisoned tag with `tag rollback`](#recovering-a-poisoned-tag-with-tag-rollback).

`AlreadyPublished` (`skipped-already-published` in the run summary and
`--summary-json` output) fires when a publisher's [`reconcile()`](#convergent-re-run)
found this exact version already landed upstream — see Convergent re-run
above for the full skip/abort table.

`ConfigSkipped` (`skipped-config` in the run summary and `--summary-json`
output) fires when a publisher's config block exists but every entry
evaluates skip-inactive right now — `skip:`/`skip_upload:` truthy or `if:`
falsy on all of them, including with `--crate <name>` selection narrowed to
one of those inactive entries. Distinct from `NotConfigured` (the block is
absent entirely): a `ConfigSkipped` publisher was registered but had nothing
active to publish, so it is recorded and never reaches `run()` — never
reported as `Succeeded` for work it never did.

`VerifyGateBlocked` (`skipped-verify-gate-blocked` in the run summary and
`--summary-json` output) fires when the pre-submitter verify-release gate
held a one-way-door publisher: after the reversible Assets and Manager
groups have already dispatched, the gate checks the just-published release's
asset content before any Submitter-group publisher (cargo, npm, PyPI,
chocolatey, winget, …) is allowed to run. A failed check, or the check
itself erroring, blocks every Submitter-group publisher this run — never
selectively.

Stage-level statuses on the run summary (printed at end-of-pipeline):

```
pending-moderation       (chocolatey awaiting moderation queue)
pending-validation       (winget PR awaiting validation pipeline)
announce-gated           (announce step skipped by announce.gate_on)
```

## Announce gating

Whether the announce step fires is governed by `announce.gate_on`:

```yaml
announce:
  gate_on: required_publishers   # required_publishers | all_publishers | none
```

| Value | Semantics |
|---|---|
| `required_publishers` (default) | Announce runs only if every `required: true` publisher succeeded. |
| `all_publishers` | Announce runs only if every configured publisher succeeded. |
| `none` | Announce always runs. |

When announce is skipped by the gate, the run summary records `announce-gated`.

## Worked example: partial failure

Scenario: a release with github-release (Assets, required), cloudsmith (Assets),
homebrew (Manager), and cargo (Submitter, required). The homebrew tap rejects
the push because branch protection got tightened.

Run:

```bash
anodizer release --summary-json=dist/run-summary.json
```

Timeline:

1. Assets group dispatches. github-release uploads tag + assets (`Succeeded`).
   cloudsmith uploads the deb (`Succeeded`).
2. Manager group dispatches. homebrew push fails (`Failed`).
3. Submitter gate evaluates. Every `required: true` Assets/Manager publisher
   succeeded; homebrew's failure is non-required, so the gate opens.
4. Submitter group dispatches. cargo publishes (`Succeeded`).
5. No automatic rollback ever fires — this run had no required-publisher
   failure to unwind, and there is no in-process rollback machinery left to
   invoke even if it had.
6. Announce step evaluates `announce.gate_on=required_publishers`. Every
   required publisher succeeded; announce runs.

Resulting `dist/run-summary.json` (abbreviated):

```json
{
  "tag": "v0.2.1",
  "submitter_gated": false,
  "announce_gated": false,
  "results": [
    { "name": "github-release", "group": "Assets", "required": true,  "outcome": "Succeeded" },
    { "name": "cloudsmith",     "group": "Assets", "required": false, "outcome": "Succeeded" },
    { "name": "homebrew",       "group": "Manager","required": false, "outcome": { "Failed": "tap push rejected: branch protection" } },
    { "name": "cargo",          "group": "Submitter","required": true,"outcome": "Succeeded" }
  ]
}
```

Contrast: if homebrew had been marked `required: true`, the Submitter gate
would have closed before cargo dispatched. `cargo` would appear as
`{ "Skipped": "SubmitterGated" }` and announce would be `announce-gated`.
Recovery is still just re-running the identical command once the branch
protection rule is fixed: github-release PATCHes the release it already
created and cloudsmith skips every file whose md5 already matches, homebrew
retries, and cargo dispatches for the first time once the gate opens.

### Recovery flow

When a release fails partway, anodizer persists the end-of-pipeline
state to `dist/run-<id>/report.json`, and re-running `release` against the
same tag is **safe by construction** — see
[The recovery model](#the-recovery-model) for the command and the
converging transcript.

```text
# A failed release leaves its report on disk:
$ ls dist/run-v0.2.1/
report.json  summary.json

# The re-run consults it before any network probe:
$ anodizer release --verbose
   • cargo: ledger fast-path — prior summary digests matched, skipping network reconcile probe
   • skipping cargo — already published for this version (all 3 planned crate(s) already on crates.io with verified content)
```

Reconciliation fails *toward* publishing: a publisher only skips on a
full positive match of name **and** version upstream. An unreachable
registry, an ambiguous response, or a partial match all dispatch the
publish attempt rather than assume success. The one hard stop is a
version already published upstream with **different content** — that
records a publisher failure telling you to bump the version, because no
amount of re-running can overwrite an immutable release.

### Recovering a poisoned tag with `tag rollback`

`anodizer tag rollback` is the inverse of `anodizer tag`: when a downstream
release fails (publish error, mcp 422, an irreversible Submitter blows up),
the operator is left with a tag pointing at a bumped-but-broken commit. The
subcommand deletes the anodize-managed tag(s) at that SHA, reverts the bump
commit, and pushes the revert — restoring the branch to a clean state so the
next `anodizer tag` invocation can re-cut from the fixed commit.

```bash
# Rollback the bump at the current HEAD (or any SHA you pass explicitly):
anodizer tag rollback "$GITHUB_SHA"

# Dry-run first:
anodizer tag rollback --dry-run "$GITHUB_SHA"

# Don't push — just mutate locally:
anodizer tag rollback --no-push "$GITHUB_SHA"
```

**Flag matrix:**

| Flag | Default | Description |
|---|---|---|
| `<SHA>` (positional) | `HEAD` | Target commit. Tags at this SHA are deleted; the commit itself is reverted (or reset past, with `--mode=reset`) |
| `--dry-run` | off | Print what would happen — no tag delete, no commit, no push |
| `--no-push` | off | Mutate locally; skip the remote tag-delete and revert-commit push |
| `--scope` | `all` | `all` (lockstep + per-crate) \| `lockstep` (`vX.Y.Z` only) \| `per-crate` (`<crate>-vX.Y.Z` only) |
| `--mode` | `revert` | `revert` (history-preserving `git revert --no-edit`, default) \| `reset` (history-rewriting `git reset --hard <sha>~1`; requires force-push to land) |
| `--force` | off | Override the published-state guard (below). For operators who are CERTAIN nothing irreversible shipped — e.g. offline recovery of a release that died before publish |
| `--branch` | auto | Branch to push the revert to. Auto-resolved from `git branch -r --contains <bump_sha>` so the bump SHA itself (not "the default branch right now") drives the lookup — race-immune to default-branch movement. Falls back to `HEAD` resolution for local-only repos. Pass `--branch` to override |

**SHA-derivation:** the bump SHA is the anchor for both the tag lookup AND
the branch resolution. There is no `--default-branch` flag and no API call
to `repos/<owner>/<repo>` — the rollback can run on a detached HEAD as long
as the bump SHA is reachable from at least one remote branch.

**Published-state guard:** before touching anything (including in
`--dry-run`), rollback checks whether the version is already burned at a
one-way-door publisher, by evidence strength:

1. **Run summaries** (`<dist>/run-*/summary.json`, per-crate
   `<dist>/<crate>/run-*/summary.json`) whose `tag` matches a tag being
   rolled back. A landed Submitter-group publisher → refuse, naming the
   publishers; only-reversible publishers → proceed to the next layer.
2. **crates.io index probe** — for every tag whose crate tag family (from
   the config's `tag_template`s) maps to a crates.io-targeting
   `publish.cargo` crate. The run summary answers a *per-run* question;
   whether a version is burned on a one-way-door registry is **global**
   state — a PRIOR run may have published it, and that run's summary lives
   on another runner's disk. The crate's exact `name@version` live on the
   sparse index → refuse (fix forward); an **unreachable index** → refuse
   (fail closed: publication state is unverifiable). A **missing or
   unparseable config** also refuses — the config is the probe's
   tag→crate mapping, so proceeding without it would blind the guard —
   as does a tag the config **cannot map to any crate** while other
   crates do target crates.io. Tags whose mapped crates simply don't
   publish to crates.io proceed (no cargo one-way door exists); crates on
   a custom `registry:`/`index:` are out of the probe's scope, exactly
   like the publish stage's own content guard.
3. **GitHub release probe** — only for tags with NO summary on disk. A
   published (non-draft) release → refuse. An **unanswerable probe**
   (gh missing, auth/network error) also refuses — fail closed: with no
   summary and no probe answer there is zero evidence the version is safe
   to destroy. An **unresolvable `origin`** (none configured, or git
   erroring) refuses for the same reason. The single fail-open bound: a
   resolvable origin that is not `github.com`-shaped (GitLab, Gitea, a
   file path, a GitHub Enterprise host) proceeds with a warning — the
   probe targets the github.com Releases API, which cannot host a release
   for such a remote, so run summaries are the only evidence layer there.

`--force` overrides the whole guard for genuinely-offline recovery.

**Safety check:** under the default `--mode=revert`, anodize hard-fails when
non-bump commits sit between HEAD and the target SHA. (Anodize's own prior
revert commits — those with the `Revert "chore(release): ` prefix — are
recognised so re-runs of the same rollback are idempotent.) Use
`--mode=reset` to force history rewrite when you genuinely want the
intervening commits gone too.

**Workflow integration:** none needed, and none automatic. `anodizer release`
never invokes `tag rollback` itself — `on_failure: hold` is the only
behavior, so a failed run always leaves the tag exactly where it is. `tag
rollback` is a **manual, deliberate** command: run it from an operator shell
(or a one-off `workflow_dispatch` job) when you have decided a release
should not exist, not merely that a step failed (a step failure recovers by
[re-running the same command](#convergent-re-run)). Workflows that wire a
custom destructive step must still gate it on the action's
`irreversibly_published` output (see above) so nothing ever tears down a
live release automatically.

**Publisher unwind:** deleting the tag and reverting the bump commit is
half of `tag rollback`'s job. The other half is unwinding whatever already
published for that tag — each Assets/Manager publisher recorded as
`Succeeded` in that run's `report.json` gets its `rollback()` override
invoked:

```
github-release  delete release + delete uploaded assets (tag refs untouched)
dockerhub       PATCH the repo description back to the pre-publish snapshot
artifactory     parallel HTTP DELETE per uploaded URL (404/410 treated as already-absent)
uploads         HTTP DELETE per recorded upload URL
cloudsmith      DELETE per package slug; warn-only checklist when CLOUDSMITH_API_KEY is unset
blob            delete each object actually written (post-upload evidence snapshot)
homebrew/scoop/nix/aur  re-clone, git revert HEAD --no-edit, git push
krew            list open PRs by head=<fork>:<branch>, PATCH state=closed per match
schemastore     close the SchemaStore PR anodizer opened
mcp             PATCH the server's registry status; degrades to a warn on rejection
gemfury         DELETE each pushed package version
cargo           cargo yank (version stays reserved; consumers cannot install fresh)
npm             npm unpublish per target; warn once outside npm's unpublish window
homebrew-core   close the bump PR; warn when the bump landed as a direct commit
pypi / chocolatey / winget / snapcraft / upstream-aur  warn-only (no programmatic path)
```

The published-state guard above still applies first: a Submitter-group
publisher that already landed (crates.io, npm, PyPI, chocolatey, winget,
snapcraft, ...) blocks the whole rollback unless `--force` is passed, because
those registries never reopen the version slot — `cargo yank` and
`npm unpublish` are best-effort withdrawals, not un-publishes.

Any publisher can opt out of the unwind with a per-block flag:

```yaml
publish:
  homebrew_cask:
    retain_on_rollback: true   # homebrew tap survives a `tag rollback` unwind
  cargo:
    retain_on_rollback: false  # (default) cargo yank still runs
```

When `retain_on_rollback: true`, `tag rollback` logs one line and moves on,
leaving that publisher's outcome exactly as the release recorded it:

```text
$ anodizer tag rollback
   • skipped rollback for 'homebrew' — retain_on_rollback is set
```

Use this when the cost of undoing a publisher is higher than the cost of
leaving it in place (e.g. a Homebrew tap PR that has already been merged
upstream).

Each unwound publisher fires its [`on_rollback` hook](#on-rollback-hooks)
(below) once the revert attempt completes, successful or not.

You never name the run. A release writes its state to
`<dist>/run-<tag>/report.json` (`<dist>/<crate>/run-<tag>/` in a per-crate
workspace), so the tags `tag rollback` already resolved from the target SHA
name their own run dirs — both layouts are swept, and there is no
`--from-run` flag to get wrong. What follows from that:

| Situation | What happens |
|---|---|
| Run dir present with recorded state | Publishers are unwound, and the pass rewrites `<dist>/run-<tag>/rollback.json` — re-running `tag rollback` resumes from it rather than re-reverting a publisher twice |
| No run dir (fresh checkout, dist never preserved) | Nothing to unwind; the git half proceeds. The published-state guard still probes the registries and the GitHub Releases API, so an unattributed live release still refuses |
| Run dir present but its state is unreadable | **Refusal.** The withdrawal cannot be completed, so the tag documenting the published state is not destroyed. Fix or delete the file, or pass `--force` |
| `--dry-run` | Prints `(dry-run) would withdraw N publisher(s) for <tag>: …` and touches nothing — no `rollback()` call, no `rollback.json` |
| `--force` | Skips the published-state guard, and downgrades an unreadable-state refusal to a warning |

The unwind runs **after** the published-state guard (a burned version must
refuse before anything is withdrawn) and **before** the tags are deleted (the
github-release publisher's own rollback reads the release that `delete_tags`
is about to remove).

Most publishers are idempotent on re-run: they detect that the current
version was already published and record a `skipped-already-published`
outcome instead of duplicating work. This covers cargo (crates.io index
check), chocolatey (feed hash), the MCP registry (duplicate-version
rejection → skip), snapcraft (existing Snap Store revision for the version →
skip), artifactory (matching sha256 already at the path → skip; a *differing*
artifact errors unless `overwrite: true`), blob (byte-identical object already
present → skip), and announce (per-version sent-marker so each channel posts
at most once).

PR-based publishers that open a pull request (homebrew, scoop, nix, krew,
homebrew-core, winget) converge through the same reconcile pass: each looks
for an already-open PR titled for this package and version at the upstream
index repo and skips when it finds one, so a re-run cannot stack duplicate
pull requests against the same tag.

## CLI surface summary

```
anodizer release \
  --fail-fast \
  --no-gate-submitter \
  --strict \
  --summary-json=<path>
```

| Flag | Semantics | Default |
|---|---|---|
| `--fail-fast` | First publisher failure aborts `PublishStage`. Nothing reaches the Submitter gate. | off |
| `--no-gate-submitter` | Disables the Submitter gate. Submitter group dispatches even when required Assets/Manager publishers failed. | gate on |
| `--strict` | Config + preflight strictness (unchanged from prior versions). | off |
| `--summary-json=<path>` | Write the per-publisher run summary JSON to this path. | `<dist>/run-<id>/summary.json` on real releases; unset (no write) for `--snapshot` / `--dry-run` |

There is no `--rollback`, `--rollback-only`, or `--from-run` flag. Recovery
is `anodizer release` (re-run to converge) or `anodizer tag rollback`
(deliberate withdrawal) — see [Convergent re-run](#convergent-re-run) and
[Recovering a poisoned tag](#recovering-a-poisoned-tag-with-tag-rollback).

## `anodizer notify`

Send a message through configured announce integrations without running a release:

```bash
# Fire all configured integrations:
anodizer notify "hotfix deployed: v0.8.1"

# Fire only specific integrations:
anodizer notify "deploy started" --publishers=slack,discord

# Omit an integration:
anodizer notify "v0.8.1 is live" --skip=webhook

# Send untrusted text literally (no Tera rendering) — e.g. from an on_error hook:
anodizer notify --raw "publish failed: $ANODIZER_ERROR"

# Opt out of outbound-body redaction for a trusted private channel:
anodizer notify --allow-secrets "deploy key rotated: $NEW_KEY"
```

| Flag | Semantics |
|---|---|
| `<message>` (positional) | Message body. Supports Tera templates — `{{ .Version }}`, `{{ .ProjectName }}`, etc. |
| `--publishers=<list>` | Comma-separated integration names to fire. Default: all configured. |
| `--skip=<list>` | Comma-separated integration names to omit. |
| `--raw` | Send the message literally, without Tera rendering. Controls **rendering only** — use it when the message contains untrusted text (e.g. error output in an `on_error` hook) so the body is not re-rendered. It does **not** control redaction. |
| `--allow-secrets` | Disable redaction of the **outbound body**, sending known secret values in plaintext. For a deliberately trusted private channel only. anodizer's own log/stderr output stays redacted regardless. See [Notification secret redaction](#notification-secret-redaction). |
| `--dry-run` | Print what would be sent; do not call external APIs. |

`anodizer notify` reads the same `announce:` config block as `anodizer release`.
No idempotency sent-marker is written — repeated `notify` calls fire every time.

## Notification secret redaction

Every outbound announce notification body — from both `anodizer notify` and
the release pipeline's `announce` stage — has known secret env values masked
before it is sent. This is the same redaction anodizer applies to its own
logs: a secret env value is replaced with `$VAR_NAME` (a real `ghp_…` token
becomes `$GITHUB_TOKEN`). Redaction is on by default; no secret value can
leak into a notification unless you explicitly opt out.

### Two redaction surfaces

- **Outbound body** (what the channel receives): redacted by default;
  `--allow-secrets` opts out.
- **anodizer's own logs / stderr** (what lands in GitHub Actions logs):
  redacted **always**, with no opt-out — even under `--allow-secrets`.

### Control matrix

`--raw` (rendering) and `--allow-secrets` (redaction) are **independent
axes** — neither flag affects the other:

| flags | Tera on body | outbound body | GitHub Actions log |
|---|---|---|---|
| (none) | rendered | redacted | redacted |
| `--raw` | verbatim | redacted | redacted |
| `--allow-secrets` | rendered | plaintext | redacted |
| `--raw --allow-secrets` | verbatim | plaintext | redacted |

### Worked example

The same message, default vs. `--allow-secrets` — note that the GitHub
Actions log is redacted in both cases:

```text
$ anodizer notify "auth failed with ghp_REALSECRET"
  → webhook receives:  auth failed with $GITHUB_TOKEN    (redacted, default)
  → GitHub Actions log: auth failed with $GITHUB_TOKEN   (redacted)

$ anodizer notify --allow-secrets "auth failed with ghp_REALSECRET"
  → webhook receives:  auth failed with ghp_REALSECRET   (plaintext, intended)
  → GitHub Actions log: auth failed with $GITHUB_TOKEN   (still redacted)
```

Redaction is **surgical**: in a large error block, only the known secret
substring becomes `$NAME`; every other character prints verbatim. A
multi-line stack trace carrying one token has just that token masked, with
the rest of the trace intact.

### Static lint — `anodizer check config`

`anodizer check config` also statically warns when an announce **content**
template literally references a secret-named env var inside a `{{ }}` or
`{% %}` block. Secret-named means the var ends in `_KEY`, `_SECRET`,
`_PASSWORD`, or `_TOKEN`. The lint covers the content surfaces a reader
would template — message / title / subject / body, Slack blocks &
attachments, Discord author, Reddit title / url:

```yaml
announce:
  slack:
    webhook_url: "https://hooks.slack.com/x"
    # warns — a secret-named env var templated into the body
    message_template: "deploy {{ Env.GITHUB_TOKEN }}"
```

```text
$ anodizer check config
   • validating configuration
   Warning: announce.slack.message_template references secret-named var Env.GITHUB_TOKEN; its value is masked by outbound redaction (sent as "$GITHUB_TOKEN"), so embedding it here is almost certainly a mistake — remove the reference
   • Config is valid.
```

The lint is **warning-only** and surgical about what it flags. It does
**not** fire on `{{ Tag }}`, on a normal env var such as `{{ Env.HOME }}`,
on a missing `--raw`, or on bare prose without `{{ }}` braces — only on a
secret-named env var inside a template block in an announce content field.

### Static check — workspace-membership guard

`anodizer check config` also **fails** (a hard error, not a warning) when a
crate that is a real on-disk Cargo dependency of a published crate is
missing from `crates:` entirely. It catches the class of failure where a
crate is a publish-order requirement on disk but absent from the config —
which would otherwise only surface late, mid-`cargo publish`, as a
registry-side "no matching package named ... found":

```text
$ anodizer check config
Error: crate 'anodizer-stage-install-script' is a workspace member and an
intra-workspace dependency of published crate 'anodizer', but is absent
from `crates:` (cargo will fail publishing 'anodizer')
```

It also fails when the dependency IS listed in `crates:` but has no active
cargo publisher of its own (skipped, or never configured for crates.io) —
cargo would still fail the dependent's publish because the dependency is
never uploaded to the registry first:

```text
$ anodizer check config
Error: crate 'anodizer-core' is an intra-workspace dependency of published
crate 'anodizer' but has no active cargo publisher (skipped or never
configured for crates.io) — cargo will fail publishing 'anodizer' because
'anodizer-core' is never uploaded to the registry
```

The guard reads the same `[dependencies]` / `[build-dependencies]` /
`[target.'cfg(...)'.dependencies]` scan anodizer uses to auto-derive
`depends_on` (see [Monorepo → Dependency
ordering](@/docs/advanced/monorepo.md#dependency-ordering)), so both stay in
sync with the same on-disk source of truth. It only fires for crates with an
active cargo publisher (`publish.cargo` configured and not skipped) — a
crate that never runs `cargo publish` can't be broken by a missing
workspace dependency, so it is never checked.

See also:

- [Determinism](./determinism.md) — byte-stability contract that backs safe retries when a publisher reports a byte mismatch
- [Recovery flags](./recovery-flags.md) — per-publisher conflict-resolution flags (replace_existing_draft, replace_existing_artifacts, republish_in_moderation, update_existing_pr, cloudsmith.republish)
