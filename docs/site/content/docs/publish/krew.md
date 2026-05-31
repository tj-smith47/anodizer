+++
title = "Krew"
description = "Publish kubectl plugins to the Krew index"
weight = 8
template = "docs.html"
+++

Anodizer generates [Krew](https://krew.sigs.k8s.io/) plugin manifest YAML files and pushes them to your krew-index fork repository. Krew is the plugin manager for `kubectl`, and publishing to the Krew index lets users install your plugin with `kubectl krew install <name>`.

## Classification

| Group | Required (default) | Rollback | Token scope |
|---|---|---|---|
| Manager | false | close PR (list open PRs by head=`<fork>:<branch>`, PATCH `state=closed` per match) | `GITHUB_TOKEN pull_request:write` |

See [Release resilience](../advanced/release-resilience.md) for the full classification table and the Submitter gate semantics.

## The `required:` field

Default: **`false`** — a Krew index PR failure is logged but does not fail the release.

Set `required: true` to make the release exit non-zero if this publisher fails:

```yaml
crates:
  - name: kubectl-mytool
    publish:
      krew:
        repository:
          owner: myorg
          name: krew-index
        short_description: "A kubectl plugin for managing things"
        required: true
```

See [Publish overview — the `required:` field](../) for the full semantics.

## Minimal config

```yaml
crates:
  - name: kubectl-mytool
    publish:
      krew:
        repository:
          owner: myorg
          name: krew-index
        short_description: "A kubectl plugin for managing things"
```

`repository` is required. The plugin manifest needs a description and a
`short_description`; both derive from the crate's `Cargo.toml`
`[package].description` when omitted (`short_description` falls back to the
description), so a plain Rust crate supplies only `repository`. Set them
explicitly only to override, or if the crate has no `description`.

## Krew config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | crate name | Override the plugin name in the manifest |
| `mode` | string | `auto` | Submission path: one of `auto` \| `bot` \| `pr-direct`. See [How mode selection works](#how-mode-selection-works) |
| `ids` | list of strings | all | Build IDs filter: only include artifacts whose `id` is in this list |
| `repository` | object | **required** | Unified repository config — supports `owner`, `name`, `token`, `branch`, `git`, and `pull_request` |
| `commit_author` | object | none | Commit author name, email, and optional signing config |
| `commit_msg_template` | string | auto | Custom commit message template |
| `description` | string | Cargo `[package].description` | Full description of the kubectl plugin. Derived from `Cargo.toml`; set this if the crate has no description. |
| `short_description` | string | `description` | One-line summary of the plugin (max 255 characters). Falls back to the (possibly Cargo-derived) description when omitted. |
| `homepage` | string | inferred | Project homepage URL; falls back to `https://github.com/<owner>/<repo>` |
| `url_template` | string | release URL | Custom URL template for artifact download URLs |
| `caveats` | string | none | Post-install message shown to users after `kubectl krew install` |
| `skip` | bool or string | `false` | Skip the krew publisher entirely (no manifest generated). Accepts bool or template string. |
| `skip_upload` | bool or string | `false` | Generate the manifest but skip the upload step; `true` always skips, `"auto"` skips for pre-releases |
| `amd64_variant` | string | `"v1"` | amd64 microarchitecture variant filter (`"v1"`, `"v2"`, `"v3"`, `"v4"`) |
| `arm_variant` | string | none | ARM version filter (`"6"`, `"7"`) |
| `update_existing_pr` | bool or string | `false` | Force-push to an existing open PR branch instead of skipping. See [Existing PR behavior](#existing-pr-behavior) and [Recovery flags](../advanced/recovery-flags.md#update_existing_pr-winget-krew-homebrew-cask). |

All string fields support Tera template rendering (e.g. `{{ ProjectName }}`, `{{ Version }}`).

## Manifests repo setup

Krew plugins are distributed through the [krew-index](https://github.com/kubernetes-sigs/krew-index) repository. To publish your plugin:

1. **Fork** the `kubernetes-sigs/krew-index` repository to your GitHub account or organization.
2. Set `repository.owner` and `repository.name` to point to your fork; configure the PR target with `repository.pull_request.base`.
3. Anodizer clones the fork, writes the manifest into the `plugins/` directory, commits to a versioned branch (`<name>-v<version>`), pushes, and opens a pull request against the upstream krew-index.

```yaml
krew:
  repository:
    owner: myorg
    name: krew-index
    pull_request:
      enabled: true
      draft: false
      base:
        owner: kubernetes-sigs
        name: krew-index
        branch: master
  short_description: "A kubectl plugin"
  description: "Full description of the plugin"
```

## Full config reference

```yaml
crates:
  - name: kubectl-mytool
    publish:
      krew:
        name: ""                          # override plugin name (default: crate name)
        short_description: "..."          # max 255 chars; falls back to description if omitted
        description: "..."               # full description; derived from Cargo.toml description if omitted
        homepage: ""                     # falls back to github.com/<owner>/<repo>
        url_template: ""                 # override download URL template
        caveats: ""                      # post-install message
        ids: []
        amd64_variant: "v1"             # v1 | v2 | v3 | v4
        arm_variant: ""                  # "6" | "7"
        repository:
          owner: myorg                   # required
          name: krew-index               # required
          token: ""                      # falls back to GITHUB_TOKEN
          branch: ""
          pull_request:
            enabled: true
            draft: false
            base:
              owner: kubernetes-sigs
              name: krew-index
              branch: master
        commit_author:
          name: ""
          email: ""
        commit_msg_template: ""
        update_existing_pr: false        # force-push to existing PR branch
        skip: false
        skip_upload: false               # bool | "auto"
```

## Authentication

Anodizer resolves a GitHub token from the `repository.token` field, or falls back to the `GITHUB_TOKEN` / `ANODIZER_FORCE_TOKEN` environment variables. The token must have push access to your krew-index fork.

## How plugin manifests are generated

Anodizer discovers all build artifacts for the crate (filtered by `ids`, `amd64_variant`, and `arm_variant` if set), then generates a Krew plugin manifest YAML file conforming to the `krew.googlecontainertools.github.com/v1alpha2` API.

Each artifact becomes a platform entry with:
- **selector**: `matchLabels` for `os` (linux, darwin, windows) and `arch` (amd64, arm64)
- **uri**: download URL for the archive
- **sha256**: checksum of the archive
- **bin**: binary name

When an artifact has arch `"all"`, it is expanded into separate entries for both `amd64` and `arm64`.

### Example generated manifest

```yaml
apiVersion: krew.googlecontainertools.github.com/v1alpha2
kind: Plugin
metadata:
  name: kubectl-mytool
spec:
  version: v1.0.0
  homepage: https://github.com/myorg/mytool
  shortDescription: A kubectl plugin for managing things
  description: A full description of what the plugin does.
  platforms:
  - selector:
      matchLabels:
        os: linux
        arch: amd64
    uri: https://github.com/myorg/mytool/releases/download/v1.0.0/mytool-1.0.0-linux-amd64.tar.gz
    sha256: deadbeefcafebabe...
    bin: kubectl-mytool
  - selector:
      matchLabels:
        os: darwin
        arch: arm64
    uri: https://github.com/myorg/mytool/releases/download/v1.0.0/mytool-1.0.0-darwin-arm64.tar.gz
    sha256: cafebabe12345678...
    bin: kubectl-mytool
```

The manifest is written to `plugins/<name>.yaml` in the krew-index repository.

## Existing PR behavior

When `gh pr create` reports that a PR for the same head branch already exists,
Anodizer's default is to **skip and emit a warning**:

```
krew: PR for 'owner:kubectl-mytool-v1.2.3' already exists — skipping
      (set update_existing_pr: true to update the PR in place)
```

Setting `update_existing_pr: true` force-pushes the updated manifest to the
existing branch using `--force-with-lease`, so the open PR automatically picks
up the new content without creating a duplicate PR:

```yaml
krew:
  update_existing_pr: true
```

## Common gotchas

- **`repository` is required**: the description and `short_description` derive from the crate's `Cargo.toml` `[package].description`, so the manifest only hard-errors if `repository` is missing or the crate has no description to fall back on.
- **PR-based submission**: the krew-index is managed via PR, not direct push. Anodizer creates a PR against `kubernetes-sigs/krew-index` from your fork. PR review and merge are manual.
- **Version updates are self-contained**: once a plugin is in krew-index, anodizer submits each version bump directly via the hosted krew-release-bot webhook — no separate workflow step. See [Version updates](#version-updates-self-contained-no-extra-workflow-step).
- **Duplicate PRs**: if a prior run already opened a PR for the same tag, use `update_existing_pr: true` to force-push instead of opening a second PR.

## Custom URL templates

Use `url_template` to override the default release download URLs:

```yaml
krew:
  url_template: "https://cdn.example.com/{{ ProjectName }}/{{ Version }}/{{ ProjectName }}-{{ Os }}-{{ Arch }}.tar.gz"
  repository:
    owner: myorg
    name: krew-index
  short_description: "A kubectl plugin"
  description: "Full plugin description"
```

## Full example

```yaml
crates:
  - name: kubectl-mytool
    publish:
      krew:
        repository:
          owner: myorg
          name: krew-index
        short_description: "Manage Kubernetes resources efficiently"
        description: "A kubectl plugin that provides advanced resource management with filtering, bulk operations, and dry-run support."
        homepage: "https://github.com/myorg/mytool"
        caveats: "Run 'kubectl mytool init' after installation to configure defaults."
        commit_msg_template: "Update {{ .Name }} plugin to {{ .Version }}"
        commit_author:
          name: "Release Bot"
          email: "bot@example.com"
        ids:
          - mytool
        amd64_variant: "v1"
        skip_upload: auto
```

## Version updates — self-contained, no extra workflow step

After your initial krew-index PR is approved and merged, every subsequent
version bump is a mechanical update the krew maintainers run through a hosted
service ([krew-release-bot](https://github.com/rajatjindal/krew-release-bot)):
it forks krew-index and opens the version-bump PR server-side, under the bot's
own GitHub account. **Anodizer drives that service directly — `anodize release`
completes the krew-index submission itself, with no separate GitHub-Actions
step and no extra token.**

### How mode selection works

The krew `mode` field selects the submission path. It defaults to `auto`:

```yaml
publish:
  krew:
    mode: auto   # auto (default) | bot | pr-direct
```

| `mode` | Behavior |
|---|---|
| `auto` (default) | Probe krew-index membership and pick the flow (see below). |
| `bot` | Always POST to the krew-release-bot webhook. Skips the probe — use when the plugin is known to be in krew-index. |
| `pr-direct` | Always open a fork PR against krew-index. Skips the probe — use for the initial submission, or a self-hosted krew-index mirror the hosted bot can't reach. |

In `auto`, anodizer makes one probe — a GET against
`api.github.com/repos/kubernetes-sigs/krew-index/contents/plugins/<name>.yaml`
(200 = published, 404 = not yet) — and picks the flow:

| In krew-index | Flow | Behavior |
|---|---|---|
| No (404) | `pr-direct` | Clone a fork, write `plugins/<name>.yaml`, open the **initial** PR against krew-index. A human reviews + merges it. |
| Yes (200) | `bot-webhook` | POST the fully-rendered manifest + the release tag to the krew-release-bot webhook, which opens the version-bump PR on your behalf. |

The probe is **authenticated** whenever a GitHub token is available
(`ANODIZER_GITHUB_TOKEN` / `GITHUB_TOKEN`, or `repository.token`) — the same
token the GitHub release uses — which raises the API rate limit from 60/hr
(anonymous) to 5,000/hr.

**Indeterminate probe → loud failure, not a guess.** If the probe can't reach a
definitive 200/404 (rate-limit, network blip, unexpected status), anodizer
**hard-errors** rather than guessing. It does **not** silently fall back to
`pr-direct`: a plugin already in krew-index wrongly routed into a fork PR is
rejected by krew maintainers. The error tells you to retry, supply a token, or
set `mode: bot` / `mode: pr-direct` explicitly to bypass the probe.

### The webhook submission

In `bot-webhook` mode anodizer POSTs a `ReleaseRequest` to the hosted endpoint:

```
POST https://krew-release-bot.rajatjindal.com/github-action-webhook
Content-Type: application/json
{
  "tagName":            "v1.2.3",
  "pluginName":         "kubectl-mytool",
  "pluginOwner":        "myorg",
  "pluginRepo":         "mytool",
  "pluginReleaseActor": "<github login>",
  "templateFile":       ".krew.yaml",
  "processedTemplate":  "<base64 of the rendered manifest>"
}
```

- The endpoint is overridable via `KREW_RELEASE_BOT_WEBHOOK_URL` (for a
  self-hosted bot deployment).
- The server forks krew-index and opens the PR under its own account — **no
  token is sent**.
- `processedTemplate` carries the **final rendered manifest**, with real
  sha256 digests already filled in. The server validates the manifest and
  commits these bytes to its krew-index fork **verbatim** — it does not fetch
  release assets or recompute shas. The `pluginOwner`/`pluginRepo`/`tagName`
  identify the submission in the PR's provenance.

The krew publisher runs **after** the GitHub Release is created and its assets
are uploaded (the `release` stage precedes the `publish` stage in the
pipeline), so the manifest's download URLs resolve by the time the PR merges.

### Idempotency + failures

- **HTTP 200** → success; the submitted PR URL is logged.
- **Already submitted** (the version was POSTed on a prior run — the bot's
  duplicate-PR / clean-working-tree response) → treated as an idempotent
  no-op success.
- **Any other failure** (non-200, network error) → the release fails loudly.
  The krew submission is never silently skipped.

> **Note:** when a PR for this version already exists, re-running does **not**
> guarantee the open PR reflects your latest submission. The server commits the
> new bytes to its fork, but anodizer can't observe the resulting PR content, so
> treat the first successful submission as authoritative.

### Rollback semantics

`pr-direct` mode rolls back by closing the PR anodizer opened. `bot-webhook`
mode has nothing for anodizer to roll back — the krew-release-bot server owns
the krew-index PR, so no rollback evidence is recorded for that flow.

## Dry-run mode

When running with `--dry-run`, Anodizer prints the plugin manifest it would generate and the target repository without cloning, committing, or pushing.
