+++
title = "Homebrew"
description = "Generate Homebrew formulae and push to tap repositories"
weight = 3
template = "docs.html"
+++

Anodizer generates Ruby Homebrew formulae with multi-platform support and pushes them to your tap repository.

## Classification

| Group | Required (default) | Rollback | Token scope |
|---|---|---|---|
| Manager | false | re-clone tap, `git revert HEAD --no-edit`, push | `GITHUB_TOKEN contents:write` |

See [Release resilience](../advanced/release-resilience.md) for the full classification table and the Submitter gate semantics.

## Minimal config

```yaml
crates:
  - name: myapp
    publish:
      homebrew:
        repository:
          owner: myorg
          name: homebrew-tap
```

## Homebrew config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `repository.owner` | string | — | GitHub owner of the tap repo |
| `repository.name` | string | — | Tap repository name |
| `folder` | string | `Formula` | Folder within the tap repo |
| `description` | string | none | Formula description |
| `license` | string | none | License identifier |
| `install` | string | auto | Custom install block (Ruby) |
| `test` | string | none | Custom test block (Ruby) |

## Full config reference

```yaml
crates:
  - name: myapp
    publish:
      homebrew:
        repository:
          owner: myorg          # required
          name: homebrew-tap    # required
          token: ""             # falls back to GITHUB_TOKEN
          branch: ""            # default: repo default branch
          pull_request:
            enabled: false
            draft: false
            base:
              owner: ""
              name: ""
              branch: ""
        folder: Formula         # subdirectory in the tap
        description: ""
        license: ""
        install: ""             # custom Ruby install block
        test: ""                # custom Ruby test block
        skip_upload: false      # bool or "auto" (skip prereleases)
        cask:                   # per-crate cask config (same shape as homebrew_casks[])
          update_existing_pr: false
```

## Homebrew Cask config fields

Casks are configured under `publish.homebrew.cask:` (per-crate) or `homebrew_casks:` (top-level array). Both axes use the same `HomebrewCaskConfig` shape.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | crate name | Cask name |
| `repository` | object | — | Tap repo (`owner`, `name`, `token`, `branch`, `pull_request`) |
| `directory` | string | `Casks` | Subdirectory in the tap for the `.rb` file |
| `app` | string | none | macOS `.app` bundle name |
| `binaries` | list | none | Binary stubs to install |
| `description` | string | none | Cask description |
| `homepage` | string | none | Project homepage |
| `skip_upload` | bool or string | `false` | Skip publishing; `true` always skips, `"auto"` skips for prereleases |
| `update_existing_pr` | bool or string | `false` | Force-push to an existing open PR branch instead of skipping. See [Cask existing PR behavior](#cask-existing-pr-behavior). |

## Authentication

| Variable | Description |
|----------|-------------|
| `GITHUB_TOKEN` | Token with push access to your tap repository (and `pull_request:write` for cask PR mode) |

The token can also be set via `repository.token` in the config.

## Common gotchas

- **Branch protection**: if your tap repo has branch protection enabled, direct push will fail. Use a fork + PR workflow via `repository.pull_request`.
- **Multiple platforms**: anodizer auto-generates `on_macos` / `on_linux` / `on_intel` / `on_arm` stanzas from the build targets. If your build only produces one platform, the formula contains a single `url` block instead of the multi-platform form.
- **Cask vs formula**: formulae install from archives; casks install macOS `.app` bundles. A crate can have both if `publish.homebrew` (formula) and `publish.homebrew.cask` (or top-level `homebrew_casks:`) are both configured.

## Republish / update behavior

Not applicable — formula files are updated in-place on each release. Re-cutting the same version overwrites the formula in the tap (prior commit stays in git history). The Manager group rollback reverts via `git revert HEAD --no-edit` + push.

For casks, set `update_existing_pr: true` to force-push to an existing open PR rather than opening a duplicate. See [Cask existing PR behavior](#cask-existing-pr-behavior).

## Cask existing PR behavior

When `gh pr create` reports a PR for the same head branch already exists,
Anodizer's default is to **skip and emit a warning**:

```
homebrew cask: PR for 'owner:myapp-cask-1.2.3' already exists — skipping
               (set update_existing_pr: true to update the PR in place)
```

Setting `update_existing_pr: true` force-pushes the updated cask file to the
existing branch using `--force-with-lease`, so the open PR picks up the new
content without creating a duplicate:

```yaml
# per-crate cask
publish:
  homebrew:
    cask:
      update_existing_pr: true

# top-level homebrew_casks array
homebrew_casks:
  - name: myapp
    update_existing_pr: true
```

## Generated formula

Anodizer generates a formula with:
- Multi-platform download URLs (`on_macos`, `on_linux`, `on_intel`, `on_arm`)
- SHA-256 checksums for each archive
- Automatic binary installation
- Package name normalization (underscores → hyphens)

## Full example

```yaml
publish:
  homebrew:
    repository:
      owner: myorg
      name: homebrew-tap
    folder: Formula
    description: "A fast CLI tool"
    license: MIT
    install: |
      bin.install "myapp"
    test: |
      system "#{bin}/myapp", "--version"
```
