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

Both `repository` and `short_description` are required. The publisher will error if either is missing.

## Krew config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | crate name | Override the plugin name in the manifest |
| `ids` | list of strings | all | Build IDs filter: only include artifacts whose `id` is in this list |
| `repository` | object | **required** | Unified repository config — supports `owner`, `name`, `token`, `branch`, `git`, and `pull_request` |
| `commit_author` | object | none | Commit author name, email, and optional signing config |
| `commit_msg_template` | string | auto | Custom commit message template |
| `description` | string | **required** | Full description of the kubectl plugin |
| `short_description` | string | **required** | One-line summary of the plugin (max 255 characters) |
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
        short_description: "..."          # required; max 255 chars
        description: "..."               # required; full description
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

- **`repository` and `short_description` are required**: omitting either causes a hard error.
- **PR-based submission**: the krew-index is managed via PR, not direct push. Anodizer creates a PR against `kubernetes-sigs/krew-index` from your fork. PR review and merge are manual.
- **krew-release-bot**: after initial merge, switch to krew-release-bot for automatic PR creation on new releases. Anodizer auto-detects when the bot is wired and writes `.krew.yaml` instead of opening a PR directly. See [Auto-promote to krew-release-bot](#auto-promote-to-krew-release-bot).
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

## Auto-promote to krew-release-bot

After your initial krew-index PR is approved and merged, the krew maintainers [recommend](https://krew.sigs.k8s.io/docs/developer-guide/release/automating-updates/) switching to [krew-release-bot](https://github.com/rajatjindal/krew-release-bot) so trivial version bumps auto-merge without manual review. **Anodizer auto-detects the switch — no config change required.**

### How auto-detection works

On every release, anodizer probes two signals:

1. **Plugin in krew-index?** — anonymous GET against `api.github.com/repos/kubernetes-sigs/krew-index/contents/plugins/<name>.yaml` (200 = published, 404 = not yet).
2. **Bot wired in this repo?** — searches `.github/workflows/*.{yml,yaml}` for the string `rajatjindal/krew-release-bot`.

The resulting mode is reported in the release summary:

| In krew-index | Bot wired | Mode | Behavior |
|---|---|---|---|
| No | — | `pr-direct` | Open PR against krew-index (initial submission). |
| Yes | No | `pr-direct-with-hint` | Open PR + log a hint about switching to the bot. |
| Yes | Yes | `bot-template` | Write `.krew.yaml`; the bot opens the krew-index PR. |

### Wiring the bot — same job, after anodizer

Add the bot step to your **existing** release workflow, **immediately after** the anodizer release step in the same job. The bot needs the GitHub Release to already be published (so its `addURIAndSha` calls can fetch each archive); running it in the same job, right after anodizer, guarantees that ordering:

```yaml
name: release
on:
  push:
    tags: ['v*']
jobs:
  release:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v6
      - uses: tj-smith47/anodizer-action@v1
        with:
          args: release
      # Bot reads the .krew.yaml that anodizer wrote in the previous step.
      - uses: rajatjindal/krew-release-bot@v0.0.46
```

**Don't put the bot in a separate workflow gated on `push: tags`** — it would race anodizer's release workflow, fire before the assets exist, and fail `addURIAndSha`.

### Alternative: separate workflow on the release event

A standalone workflow is fine if you trigger it on the release-published event so it only runs after anodizer's publish step completes:

```yaml
name: krew-release
on:
  release:
    types: [published]
jobs:
  release:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v6
      - uses: rajatjindal/krew-release-bot@v0.0.46
```

### Template path

By default anodizer writes `.krew.yaml` (the bot's default read location). To use a different path, set the bot action's `krew_template_file:` input — anodizer reads that value and writes there:

```yaml
      - uses: rajatjindal/krew-release-bot@v0.0.46
        with:
          krew_template_file: deploy/mytool.krew.yaml
```

For workspaces with multiple kubectl plugins, create a `.krew/` directory in your repo root. Anodizer detects it and writes each plugin's template to `.krew/<plugin-name>.yaml`; configure the bot action's `krew_template_file:` per plugin.

### What anodizer writes in bot-template mode

```yaml
# Generated by anodizer for krew-release-bot. DO NOT EDIT.
apiVersion: krew.googlecontainertools.github.com/v1alpha2
kind: Plugin
metadata:
  name: kubectl-mytool
spec:
  version: "{{ .TagName }}"
  homepage: https://github.com/myorg/mytool
  shortDescription: A kubectl plugin
  platforms:
    - selector:
        matchLabels:
          os: linux
          arch: amd64
      {{addURIAndSha "https://github.com/myorg/mytool/releases/download/{{ .TagName }}/mytool-{{ .TagName }}-linux-amd64.tar.gz" .TagName | indent 6}}
      bin: kubectl-mytool
```

`{{ .TagName }}` and `addURIAndSha` are bot-side placeholders — anodizer emits them literally; the bot expands them when it composes the actual krew-index manifest.

## Dry-run mode

When running with `--dry-run`, Anodizer prints the plugin manifest it would generate and the target repository without cloning, committing, or pushing.
