+++
title = "Release Resilience"
description = "Three-group publisher dispatch, Submitter gate, rollback, and replay"
weight = 6
template = "docs.html"
+++

Releases fan out to many publishers (GitHub Releases, crates.io, Homebrew taps,
Docker Hub, Cloudsmith, Artifactory, Scoop, Nix, Krew, MCP, AUR, Snapcraft,
Chocolatey, Winget, blob storage). Each has a different cost of failure. A
botched DockerHub description sync is a no-op for end users; a botched
`cargo publish` burns a version slot forever. Anodizer's release pipeline is
shaped around that asymmetry.

This guide walks through:

- The three publisher groups (Assets / Manager / Submitter) and why dispatch order matters.
- The Submitter gate that prevents irreversible publishers from firing after a required failure.
- The `--rollback` flag and per-publisher rollback shapes.
- `--fail-fast` and how it differs from the default collect-then-decide behavior.
- `--rollback-only --from-run=<id>` for replaying rollback against a prior run report.
- `--summary-json=<path>` for capturing the audit trail.
- A worked partial-failure example.

## Publisher groups

Every publisher is classified into exactly one group, based on how recoverable
a failure is:

| Group | Property | Examples |
|---|---|---|
| Assets | Writes uploadable bytes to systems we control end-to-end. Reversible via API delete. | github-release, dockerhub, artifactory, cloudsmith, blob |
| Manager | Writes to package-manager state. Server-side deletable, but consumer machines may already have pulled the artifact. | homebrew, scoop, nix, krew, mcp, our-AUR-repos, custom-publishers |
| Submitter | Writes to a third-party submission queue, an immutable registry slot, or a channel position we cannot reclaim. | cargo, chocolatey, winget, snapcraft, upstream-AUR (force-push) |

Within `PublishStage`, dispatch order is Assets, then Manager, then Submitter.
Order inside a group matches the existing (per-publisher) dispatch order.
Snapcraft stays in its own stage running after `PublishStage`; it is Submitter
group and has no rollback, so the existing stage boundary is fine.

Blob runs as its own stage between `PublishStage` and `SnapcraftPublishStage`
so that a blob upload failure can short-circuit Snapcraft via the same gate
logic.

## Per-publisher classification

| Publisher | Group | required (default) | Rollback action | Token scope |
|---|---|---|---|---|
| github-release | Assets | true | delete release + delete tag + delete assets | `contents:write` |
| dockerhub | Assets | false | manual cleanup checklist (description PATCH) | `DOCKER_TOKEN write` |
| artifactory | Assets | false | DELETE artifact path | `ARTIFACTORY_TOKEN delete` |
| cloudsmith | Assets | false | DELETE `/v1/packages/<id>` | `CLOUDSMITH_API_KEY package_delete` |
| blob (s3/gcs/azure) | Assets | false | delete object | backend creds |
| homebrew (tap) | Manager | false | git revert + push | `GITHUB_TOKEN contents:write` |
| scoop (bucket) | Manager | false | git revert + push | `GITHUB_TOKEN contents:write` |
| nix (overlay repo) | Manager | false | git revert + push | `GITHUB_TOKEN contents:write` |
| krew | Manager | false | close PR | `GITHUB_TOKEN pull_request:write` |
| mcp | Manager | false | warn-only (manual cleanup) | n/a |
| our-AUR-repos | Manager | false | git revert + push | `AUR_SSH_KEY write` |
| custom-publishers | Manager | false | none | depends on publisher |
| upstream-AUR (force-push) | Submitter | false | none | `AUR_SSH_KEY write` |
| cargo | Submitter | true | `cargo yank` (documented limits) | `CARGO_REGISTRY_TOKEN yank` |
| chocolatey | Submitter | false | none (manual withdraw) | n/a |
| winget | Submitter | false | none (manual PR close) | n/a |
| snapcraft | Submitter | false | none (already-installed snaps keep the revision) | `SNAPCRAFT_LOGIN` |

`required: true` means the release pipeline treats this publisher's failure as
fatal for downstream gating. The defaults reflect operator intent: github and
cargo must succeed for a release to mean anything; everything else is
opportunistic. Override per-publisher in your config:

```yaml
publish:
  homebrew:
    required: true     # block submitter dispatch + announce on tap failure
```

## The Submitter gate

Between Manager and Submitter dispatch, anodizer inspects the in-progress
`PublishReport`:

- If any `required: true` publisher in Assets or Manager failed, the entire
  Submitter group is skipped and each entry is recorded as
  `skipped-submitter-gated`.
- If every `required: true` Assets/Manager publisher succeeded, Submitter
  dispatch proceeds even when some `required: false` Manager publishers
  failed.

The gate is on by default. Operator opt-out:

```bash
anodize release --no-gate-submitter
```

Use this only when you have manually verified the failed publisher is not
load-bearing for the release. The default keeps you from burning a crates.io
version slot because a homebrew tap push happened to hit a branch-protection
glitch.

## The `--rollback` flag

```
--rollback={none|best-effort}
```

| Value | Behavior |
|---|---|
| `none` | No rollback runs. Failed publishers stay published; the operator handles cleanup. |
| `best-effort` | Each Assets and Manager publisher's `rollback` runs independently. Per-publisher failures are logged and the loop continues. |

Default is `best-effort` when preflight reports clean rollback scopes, `none`
otherwise (with a warning). Submitter publishers' rollback is informational
only because the underlying systems cannot reclaim the slot; the
report still records `RolledBack` or `RollbackSkippedNoScope` accordingly.

### Per-publisher rollback shapes

```
github-release  delete release + delete tag + delete uploaded assets
cargo           cargo yank (version stays reserved; consumers cannot install fresh)
dockerhub       manual cleanup checklist (description PATCH cannot be un-done programmatically)
artifactory     parallel HTTP DELETE per uploaded URL (404/410 treated as already-absent)
cloudsmith      structured warn line per (org, repo, filename) tuple (DELETE migration pending)
blob            delete each object actually written (post-upload evidence snapshot)
homebrew/scoop/nix/our-AUR  re-clone, git revert HEAD --no-edit, git push
krew            list open PRs by head=<fork>:<branch>, PATCH state=closed per match
mcp / chocolatey / winget / snapcraft / upstream-AUR  warn-only (no programmatic path)
```

### Rollback scope preflight

Each publisher declares a `rollback_scope_needed` label (the bullet list
above's "Token scope" column). Preflight surfaces missing scope as:

- A warning under default settings.
- A blocker under `--strict`.
- An immediate bail (before any publishing) when `--rollback=best-effort` is
  passed explicitly and any `required: true` publisher lacks the rollback
  scope.

## `--fail-fast` vs. default

| Mode | Behavior |
|---|---|
| Default | `PublishStage` keeps dispatching publishers after a failure. The Submitter gate evaluates the collected report and decides whether the Submitter group runs. |
| `--fail-fast` | First publisher failure aborts the stage. Nothing reaches the Submitter gate. Rollback (if enabled) still fires on what already published. |

Default mode is the right choice for most releases: it maximizes the chance of
ending up with a consistent set of Assets even if one Manager publisher
hiccups. Use `--fail-fast` only when you want loud diagnostics and have a
human ready to retry.

## `--rollback-only --from-run=<id>`

Anodizer writes a structured run report to `dist/run-<id>/report.json` after
every release attempt. `--rollback-only` re-attempts rollback against that
report:

```bash
anodize release --rollback-only --from-run=20260514T142301Z
```

What runs:

- No new publishing. No new build. No new release creation.
- For each prior `Succeeded` entry, the same publisher's `rollback` runs.
- For each `RollbackFailed` entry, the rollback is re-attempted.
- For everything else (`Skipped*`, `Failed`, already-`RolledBack`), no action.

The replay path uses the same code that drives the rollback step inside
`PublishStage`, so a green replay means every reversible publisher was
unwound. Submitter publishers print the same warn-only diagnostics they would
have written during the original run.

## `--summary-json=<path>`

Captures the audit trail of a run as a single JSON document:

```bash
anodize release --summary-json=dist/run-summary.json
```

Shape:

```json
{
  "schema_version": 1,
  "anodize_version": "0.2.1",
  "tag": "v0.2.1",
  "submitter_gated": false,
  "announce_gated": false,
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
keeps drift loud.

## The outcome set

Per-publisher `outcome` in the report uses this fixed set:

```
Succeeded
Skipped(SubmitterGated | NotConfigured | Snapshot | DryRun)
Failed(<message>)
RolledBack
RollbackFailed(<message>)
RollbackSkippedNoScope
```

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
anodize release --summary-json=dist/run-summary.json
```

Timeline:

1. Assets group dispatches. github-release uploads tag + assets (`Succeeded`).
   cloudsmith uploads the deb (`Succeeded`).
2. Manager group dispatches. homebrew push fails (`Failed`).
3. Submitter gate evaluates. Every `required: true` Assets/Manager publisher
   succeeded; homebrew's failure is non-required, so the gate opens.
4. Submitter group dispatches. cargo publishes (`Succeeded`).
5. Default `--rollback=best-effort` does not fire on a successful run; no
   rollback runs.
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
`{ "Skipped": "SubmitterGated" }`, announce would be `announce-gated`, and
running `--rollback-only --from-run=<id>` would unwind the github-release
upload (delete release + tag + assets) and the cloudsmith upload.

## CLI surface summary

```
anodize release \
  --fail-fast \
  --no-gate-submitter \
  --rollback={none|best-effort} \
  --strict \
  --simulate-failure=<publisher> \
  --rollback-only \
  --from-run=<id> \
  --allow-rerun \
  --summary-json=<path>
```

| Flag | Semantics | Default |
|---|---|---|
| `--fail-fast` | First publisher failure aborts `PublishStage`. Nothing reaches the Submitter gate. | off |
| `--no-gate-submitter` | Disables the Submitter gate. Submitter group dispatches even when required Assets/Manager publishers failed. | gate on |
| `--rollback` | `none` skips rollback; `best-effort` runs each Assets/Manager rollback independently. | `best-effort` when preflight is clean, `none` otherwise (with a warning) |
| `--strict` | Config + preflight strictness (unchanged from prior versions). | off |
| `--simulate-failure=<publisher>` | Forces a named publisher to return `Err`. Hidden flag, gated by `ANODIZE_TEST_HARNESS=1`. | unset |
| `--rollback-only` | Reads a prior run report and re-attempts rollback only. No new publishing. | n/a |
| `--from-run=<id>` | Run id whose `dist/run-<id>/report.json` to load when using `--rollback-only`. | n/a |
| `--allow-rerun` | DANGEROUS: force `release` to re-run publish even when a prior `dist/run-<id>/report.json` exists. PR-based publishers (homebrew/scoop/nix/krew/MCP) will open duplicate PRs. Prefer `--rollback-only --from-run=<id>` first. | off |
| `--summary-json=<path>` | Write the per-publisher run summary JSON to this path. | unset |

See also: [Determinism](./determinism.md) for the byte-stability contract that
backs safe retries when a publisher reports a byte mismatch.
