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
- `release.on_failure` — the in-process policy that rolls back (or holds) the tag and version bump when a run fails, with no workflow-side steps.
- `--fail-fast` and how it differs from the default collect-then-decide behavior.
- `--rollback-only --from-run=<id>` for replaying rollback against a prior run report.
- `--summary-json=<path>` for capturing the audit trail.
- A worked partial-failure example.

## Release-stage retry flags

Two flags on the `release:` block make individual release-stage runs idempotent
without requiring a full rollback:

- `release.replace_existing_draft` — DELETE-and-recreate a draft release with the same name
- `release.replace_existing_artifacts` — DELETE-and-re-upload an asset that conflicts with new bytes

Both are safe to set permanently; they are no-ops when there is no existing draft or conflicting asset.
See [Recovery flags](./recovery-flags.md) for the full mechanism, the equivalent flags on every other
publisher, and operational guidance.

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

Blob runs as its own stage BEFORE `PublishStage` (and `SnapcraftPublishStage`)
so that a required-blob upload failure is recorded in the publish report before
the Submitter gate evaluates — gating the one-way-door publishers
(cargo / chocolatey / winget) as well as Snapcraft via the same gate logic.
Ordered after `PublishStage`, a blob failure could only ever gate the
still-later Snapcraft stage while cargo / chocolatey / winget had already fired
irreversibly. Blob needs only the built dist, so running it ahead of the doors
is safe.

## Per-publisher classification

| Publisher | Group | required (default) | Rollback action | Token scope |
|---|---|---|---|---|
| github-release | Assets | true | delete release + delete tag + delete assets | `contents:write` |
| dockerhub | Assets | false | warn-only (description PATCH manual-cleanup checklist; prior description not snapshotted) | `DOCKER_TOKEN description snapshot+restore` |
| artifactory | Assets | false | DELETE artifact path | `ARTIFACTORY_TOKEN delete` |
| cloudsmith | Assets | false | DELETE `/v1/packages/<id>` | `CLOUDSMITH_API_KEY package_delete` |
| blob (s3/gcs/azure) | Assets | false | delete object | backend creds |
| homebrew (tap) | Manager | false | git revert + push | `GITHUB_TOKEN contents:write` |
| scoop (bucket) | Manager | false | git revert + push | `GITHUB_TOKEN contents:write` |
| nix (overlay repo) | Manager | false | git revert + push | `GITHUB_TOKEN contents:write` |
| krew | Manager | false | PrDirect: close the PR anodizer opened. BotWebhook: no-op (the krew-release-bot server owns the krew-index PR) | `GITHUB_TOKEN pull_request:write` (PrDirect) |
| mcp | Manager | false | warn-only (no programmatic unpublish; manual mark-deprecated via registry admin UI) | `MCP_GITHUB_TOKEN publish` |
| our-AUR-repos | Manager | false | git revert + push | `AUR_SSH_KEY write` |
| custom-publishers | Manager | false | none | depends on publisher |
| upstream-AUR (force-push) | Submitter | false | none | `AUR_SSH_KEY write` |
| cargo | Submitter | true | `cargo yank` (documented limits) | `CARGO_REGISTRY_TOKEN yank` |
| chocolatey | Submitter | false | none (manual withdraw) | n/a |
| winget | Submitter | false | warn-only (manual PR close against `microsoft/winget-pkgs`; upstream validation cannot be cancelled mid-flight) | `GITHUB_TOKEN pull_request:write` (preflight bookkeeping; warn-only at runtime) |
| snapcraft | Submitter | false | none (already-installed snaps keep the revision) | `SNAPCRAFT_LOGIN` |

`required: true` means the release pipeline treats this publisher's failure as
fatal for downstream gating. The defaults reflect operator intent: github and
cargo must succeed for a release to mean anything; everything else is
opportunistic. Override per-publisher in your config:

```yaml
publish:
  homebrew_cask:
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
anodizer release --no-gate-submitter
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

### `retain_on_rollback` — skip rollback for a specific publisher

Any publisher can opt out of rollback via a per-block flag:

```yaml
publish:
  homebrew_cask:
    retain_on_rollback: true   # homebrew tap survives a triggered rollback
  cargo:
    retain_on_rollback: false  # (default) cargo yank runs on rollback
```

When `retain_on_rollback: true` and a rollback triggers, anodizer logs a
`rollback: skipping '<name>' — retain_on_rollback is set` line and moves on.
Use this when the cost of undoing a publisher is higher than the cost of
leaving it in place (e.g. a Homebrew tap PR that has already been merged
upstream).

## `on_error` hooks

Shell hooks that fire once per FAILED publisher, after rollback has run (so
`{{ .RolledBack }}` reflects the final outcome):

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
| `ANODIZER_ROLLED_BACK` | `{{ .RolledBack }}` | `true` if any publisher was rolled back (or rollback was attempted and failed) during this run |

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

## `release.on_failure` — the in-process failure policy

When a `release` / `release --publish-only` / `release --merge` run fails,
the binary itself decides what happens next — no summary-parsing `if:` chain
is needed in workflow YAML:

```yaml
release:
  on_failure: rollback   # rollback | hold; default rollback
```

| Value | Behavior on a pipeline failure |
|---|---|
| `rollback` | Deletes the run's release tag(s) and reverts the version-bump commit — the same execution path as `anodizer tag rollback` — so the same version can be re-cut after the fix lands. |
| `hold` | Leaves tags, commits, and published state in place for forensics. Exit is still nonzero; recover with `release --rollback-only --from-run=<id>` and/or `tag rollback` once investigated. |

This policy operates on the git-level state (`tag` + bump commit). It is
independent of the per-publisher [`--rollback`](#the-rollback-flag)
machinery, which unwinds individual publishers' uploads inside the publish
stage and runs first either way.

### Automatic degrade past one-way doors

`rollback` degrades to `hold` the moment ANY one-way-door (Submitter-group)
publisher has landed — regardless of config. crates.io, chocolatey, winget,
snapcraft and friends never accept the same version twice: the version is
burned, deleting the tag could only orphan the live published state, and
fix-forward is the only path. The degrade message names the publishers that
burned the version:

```
[failure-policy] ⚠ on_failure=rollback DEGRADED to hold: one-way-door
publisher(s) already accepted this version: cargo, chocolatey. ...
Fix forward: keep the tag, revert reversible publishers with
`anodizer release --rollback-only --from-run=<id>` if needed, repair the
failure, and cut the NEXT version.
```

The evidence comes from the run's own summaries — every
`dist/run-*/summary.json` plus `dist/<crate>/run-*/summary.json`, so a crate
that published irreversibly before a later crate failed (per-crate workspace
mode) still degrades the whole run. The shared `tag rollback` path keeps its
own published-state guard as a second layer: it additionally probes the
GitHub Releases API for tags with no local summary, which is what protects
re-publish runs of an already-live release.

### Scope and recording

The policy is a root-level `release:` setting: in workspace configs
(lockstep or per-crate) the top-level `release.on_failure` governs the whole
run, and setting it in a crate-level `release:` block is a config-load
error. It does not fire for `--dry-run`, `--snapshot`, `--prepare`, `--split`,
`--announce-only`, `--rollback-only`, or `--preflight` — none of those may
destroy release state.

Whichever path runs is recorded in the run summary so the audit artifact
states how the failure was handled:

```json
"failure_policy": {
  "configured": "rollback",
  "action": "held",
  "degraded": true,
  "burned_publishers": ["cargo"]
}
```

`action` is `rolled-back`, `held`, or `rollback-failed` (rollback was
attempted but refused or errored — state is effectively held; the error text
lands in `rollback_error`). A killed run (SIGKILL, runner eviction) cannot
execute its own policy; the per-publisher summary snapshots persisted during
dispatch are the forensics trail for manual recovery in that case.

## `--rollback-only --from-run=<id>`

Anodizer writes a structured run report to `dist/run-<id>/report.json` after
every release attempt. `--rollback-only` re-attempts rollback against that
report:

```bash
anodizer release --rollback-only --from-run=20260514T142301Z
```

What runs:

- No new publishing. No new build. No new release creation.
- For each prior `Succeeded` entry, the same publisher's `rollback` runs.
- For each `RollbackFailed` entry, the rollback is re-attempted.
- For each `RollbackSkippedNoScope` entry, the rollback is re-attempted now
  that the scope env var can be exported (`retain_on_rollback` and the scope
  check are still honored; previously these rows were stranded).
- A `Failed` Submitter entry re-runs only a declared programmatic rollback
  (cargo's idempotent yank).
- For everything else (`Skipped`, already-`RolledBack`, `PendingModeration`,
  `PublishedNoRollback`), no action.

The replay path uses the same code that drives the rollback step inside
`PublishStage`, so a green replay means every reversible publisher was
unwound. Submitter publishers print the same warn-only diagnostics they would
have written during the original run.

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
  "schema_version": 1,
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
keeps drift loud.

`publishers_succeeded` / `publishers_failed` count outcomes that left durable
published state (respectively, a `failed` outcome).
`irreversibly_published` is the recovery verdict: `true` when any
Submitter-group publisher's publish landed. Submitter targets (crates.io,
chocolatey, winget, snapcraft, ...) never accept the same version twice, so
once it flips the version is burned — a tag rollback can only orphan the live
release, never enable a clean same-version re-cut. Even a `rolled-back`
Submitter counts: `cargo yank` withdraws the artifact but does not reopen the
version slot. Reversible publishers (release assets, blobs, tap/bucket/index
commits) never set it; their state is deletable and the same version can be
re-cut, so rollback stays available after they succeed.

Recovery tooling consumes the flag at two layers — both in-process by
default:

```bash
# 1. The release run itself: the in-process `release.on_failure` policy
#    degrades rollback to hold the moment the flag would flip (see above).

# 2. Manual recovery: `tag rollback` reads dist/run-*/summary.json itself
#    and refuses when the version is burned (override with --force):
$ anodizer tag rollback
Error: refusing to roll back — one-way-door publisher(s) already accepted these version(s):
  v0.8.0: version burned at cargo, chocolatey
...
Fix forward instead: keep the tag, repair the failure, and cut the NEXT version
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
anodizer release --summary-json=dist/run-summary.json
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

### Recovery flow

When a release fails partway, anodizer persists the end-of-pipeline
state to `dist/run-<id>/report.json`. The next `release` invocation
against the same tag will refuse to re-publish, citing that file:

```bash
# Failed release leaves report.json on disk:
$ ls dist/run-v0.2.1/
report.json

# Retrying with the same tag is refused — duplicate-PR risk:
$ anodizer release
Error: publish refusing to run: a prior report.json exists at
  dist/run-v0.2.1/report.json (run_id=v0.2.1). To recover from a partial
  failure, run `anodizer release --rollback-only --from-run=v0.2.1` first
  (this reverts reversible publishers and is idempotent). Pass --allow-rerun
  to force re-publish anyway — WARNING: PR-based publishers (homebrew,
  scoop, nix, krew, MCP) will open DUPLICATE pull requests against the
  same tag.
```

The recommended recovery is to unwind reversible publishers (Assets +
Manager groups) first, then fix whatever broke and re-cut the release
on a new tag:

```bash
# Step 1: replay rollback against the prior run. Idempotent: re-running
#         only re-attempts entries that haven't already RolledBack.
anodizer release --rollback-only --from-run=v0.2.1

# Step 2: read dist/run-v0.2.1/rollback.json to confirm every Assets /
#         Manager publisher flipped to RolledBack (or RollbackFailed
#         for the ones that need manual cleanup — those entries name
#         the publisher and the error).

# Step 3: cut a new tag (anodizer tag creates and pushes the next
#         semver from your commit log; release.yml triggers on the
#         pushed tag and re-runs the pipeline).
anodizer tag
```

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
   publishers; only-reversible publishers → proceed.
2. **GitHub release probe** — only for tags with NO summary on disk. A
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

**Workflow integration:** none needed. A failed `anodizer release` executes
the same rollback path itself via the in-process
[`release.on_failure` policy](#release-on-failure-the-in-process-failure-policy),
already gated on the one-way-door evidence — a workflow-level rollback step
would only race it. `tag rollback` is the **manual** recovery command: run it
from an operator shell (or a one-off `workflow_dispatch` job) when a run was
killed before it could execute its own policy, or when `on_failure: hold`
deliberately left the tag in place for forensics. Workflows that still wire a
custom destructive step must gate it on the action's `irreversibly_published`
output (see above) so a post-publish failure never triggers automated
destruction of a live release.

`tag rollback` complements `release --rollback-only` rather than replacing
it: use `--rollback-only` to unwind individual publisher state (reversible
Assets / Manager DELETEs, PR closes, blob removes); use `tag rollback` to
delete the tag itself and revert the bump commit so the next `anodizer tag`
can cut a fresh version from the fixed code.

Most publishers are idempotent on re-run: they detect that the current
version was already published and record a `skipped-already-published`
outcome instead of duplicating work. This covers cargo (crates.io index
check), chocolatey (feed hash), the MCP registry (duplicate-version
rejection → skip), snapcraft (existing Snap Store revision for the version →
skip), artifactory (matching sha256 already at the path → skip; a *differing*
artifact errors unless `overwrite: true`), blob (byte-identical object already
present → skip), and announce (per-version sent-marker so each channel posts
at most once).

PR-based publishers that open a pull request (homebrew, scoop, nix, krew) are
the remaining exception — re-running them can open a second PR against the
same tag, so they have no runtime duplicate guard.

Only use `--allow-rerun` when:

1. The recovery flow above has completed (or you've confirmed by hand
   that nothing got published on the failed run).
2. No PR-opening publisher (homebrew, scoop, nix, krew) is configured —
   re-running them can DUPLICATE the PR with no safeguard. (MCP is a
   registry POST, not a PR, and is idempotent — re-running skips an
   already-published version.)
3. You understand that an idempotent publisher will SKIP (not re-publish)
   any version it already landed on the failed run, while the PR-opening
   publishers above remain the only duplicate-publish risk.

```bash
# Escape hatch — duplicate-publish risk, see warnings above:
anodizer release --allow-rerun
```

## CLI surface summary

```
anodizer release \
  --fail-fast \
  --no-gate-submitter \
  --rollback={none|best-effort} \
  --strict \
  --rollback-only \
  --from-run=<id> \
  --allow-rerun                  # DANGEROUS — see "Recovery flow" above
  --summary-json=<path>
```

| Flag | Semantics | Default |
|---|---|---|
| `--fail-fast` | First publisher failure aborts `PublishStage`. Nothing reaches the Submitter gate. | off |
| `--no-gate-submitter` | Disables the Submitter gate. Submitter group dispatches even when required Assets/Manager publishers failed. | gate on |
| `--rollback` | `none` skips rollback; `best-effort` runs each Assets/Manager rollback independently. | `best-effort` when preflight is clean, `none` otherwise (with a warning) |
| `--strict` | Config + preflight strictness (unchanged from prior versions). | off |
| `--rollback-only` | Reads a prior run report and re-attempts rollback only. No new publishing. | n/a |
| `--from-run=<id>` | Run id whose `dist/run-<id>/report.json` to load when using `--rollback-only`. | n/a |
| `--allow-rerun` | DANGEROUS: force `release` to re-run publish even when a prior `dist/run-<id>/report.json` exists. PR-based publishers (homebrew/scoop/nix/krew/MCP) will open duplicate PRs. Prefer `--rollback-only --from-run=<id>` first. | off |
| `--summary-json=<path>` | Write the per-publisher run summary JSON to this path. | `<dist>/run-<id>/summary.json` on real releases; unset (no write) for `--snapshot` / `--dry-run` |

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

See also:

- [Determinism](./determinism.md) — byte-stability contract that backs safe retries when a publisher reports a byte mismatch
- [Recovery flags](./recovery-flags.md) — per-publisher conflict-resolution flags (replace_existing_draft, replace_existing_artifacts, republish_in_moderation, update_existing_pr, cloudsmith.republish)
