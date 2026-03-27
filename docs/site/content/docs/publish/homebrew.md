+++
title = "Homebrew"
description = "Generate Homebrew formulae and push to tap repositories"
weight = 3
template = "docs.html"
+++

Anodize generates Ruby Homebrew formulae with multi-platform support and pushes them to your tap repository.

## Minimal config

```yaml
crates:
  - name: myapp
    publish:
      homebrew:
        tap:
          owner: myorg
          name: homebrew-tap
```

## Homebrew config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `tap.owner` | string | — | GitHub owner of the tap repo |
| `tap.name` | string | — | Tap repository name |
| `folder` | string | `Formula` | Folder within the tap repo |
| `description` | string | none | Formula description |
| `license` | string | none | License identifier |
| `install` | string | auto | Custom install block (Ruby) |
| `test` | string | none | Custom test block (Ruby) |

## Generated formula

Anodize generates a formula with:
- Multi-platform download URLs (`on_macos`, `on_linux`, `on_intel`, `on_arm`)
- SHA-256 checksums for each archive
- Automatic binary installation
- Package name normalization (underscores → hyphens)

## Full example

```yaml
publish:
  homebrew:
    tap:
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
