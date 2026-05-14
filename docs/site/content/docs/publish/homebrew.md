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
