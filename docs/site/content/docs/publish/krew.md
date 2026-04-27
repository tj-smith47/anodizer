+++
title = "Krew"
description = "Publish kubectl plugins to the Krew index"
weight = 8
template = "docs.html"
+++

Anodizer generates [Krew](https://krew.sigs.k8s.io/) plugin manifest YAML files and pushes them to your krew-index fork repository. Krew is the plugin manager for `kubectl`, and publishing to the Krew index lets users install your plugin with `kubectl krew install <name>`.

## Minimal config

```yaml
crates:
  - name: kubectl-mytool
    publish:
      krew:
        manifests_repo:
          owner: myorg
          name: krew-index
        short_description: "A kubectl plugin for managing things"
```

Both `manifests_repo` (or `repository`) and `short_description` are required. The publisher will error if either is missing.

## Krew config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | crate name | Override the plugin name in the manifest |
| `ids` | list of strings | all | Build IDs filter: only include artifacts whose `id` is in this list |
| `manifests_repo.owner` | string | -- | GitHub owner of the krew-index fork |
| `manifests_repo.name` | string | -- | Repository name of the krew-index fork |
| `repository` | object | none | Unified repository config (preferred over `manifests_repo`) -- supports `owner`, `name`, `token`, `branch`, `git`, and `pull_request` |
| `commit_author` | object | none | Commit author name, email, and optional signing config |
| `commit_msg_template` | string | auto | Custom commit message template |
| `description` | string | **required** | Full description of the kubectl plugin |
| `short_description` | string | **required** | One-line summary of the plugin (max 255 characters) |
| `homepage` | string | inferred | Project homepage URL; falls back to `https://github.com/<owner>/<repo>` |
| `url_template` | string | release URL | Custom URL template for artifact download URLs |
| `caveats` | string | none | Post-install message shown to users after `kubectl krew install` |
| `skip_upload` | bool or string | `false` | Skip publishing; `true` always skips, `"auto"` skips for pre-releases |
| `upstream_repo` | object | none | Legacy PR target repo (`owner`/`name`). Prefer `repository.pull_request.base` |
| `amd64_variant` | string | `"v1"` | amd64 microarchitecture variant filter (`"v1"`, `"v2"`, `"v3"`, `"v4"`) |
| `arm_variant` | string | none | ARM version filter (`"6"`, `"7"`) |

All string fields support Tera template rendering (e.g. `{{ ProjectName }}`, `{{ Version }}`).

## Manifests repo setup

Krew plugins are distributed through the [krew-index](https://github.com/kubernetes-sigs/krew-index) repository. To publish your plugin:

1. **Fork** the `kubernetes-sigs/krew-index` repository to your GitHub account or organization.
2. Set `manifests_repo.owner` and `manifests_repo.name` to point to your fork.
3. Anodizer will clone the fork, write the manifest into the `plugins/` directory, commit to a versioned branch (`<name>-v<version>`), push, and open a pull request against the upstream krew-index.

If you use the unified `repository` config instead of `manifests_repo`, you can configure PR behavior with the `pull_request` sub-key:

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

## Custom URL templates

Use `url_template` to override the default release download URLs:

```yaml
krew:
  url_template: "https://cdn.example.com/{{ ProjectName }}/{{ Version }}/{{ ProjectName }}-{{ Os }}-{{ Arch }}.tar.gz"
  manifests_repo:
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
        manifests_repo:
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

## Dry-run mode

When running with `--dry-run`, Anodizer prints the plugin manifest it would generate and the target repository without cloning, committing, or pushing.
