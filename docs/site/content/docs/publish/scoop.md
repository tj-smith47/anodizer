+++
title = "Scoop"
description = "Generate Scoop manifests for Windows package management"
weight = 4
template = "docs.html"
+++

Anodizer generates Scoop JSON manifests and pushes them to your bucket repository.

## Classification

| Group | Required (default) | Rollback | Token scope |
|---|---|---|---|
| Manager | false | re-clone bucket, `git revert HEAD --no-edit`, push | `GITHUB_TOKEN contents:write` |

See [Release resilience](../advanced/release-resilience.md) for the full classification table and the Submitter gate semantics.

## Minimal config

```yaml
crates:
  - name: myapp
    publish:
      scoop:
        repository:
          owner: myorg
          name: scoop-bucket
```

## Scoop config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `repository.owner` | string | — | GitHub owner of the bucket repo |
| `repository.name` | string | — | Bucket repository name |
| `description` | string | none | Manifest description |
| `license` | string | none | License identifier |

## Full config reference

```yaml
crates:
  - name: myapp
    publish:
      scoop:
        repository:
          owner: myorg              # required
          name: scoop-bucket        # required
          token: ""                 # falls back to GITHUB_TOKEN
          branch: ""                # default: repo default branch
        description: "A fast CLI tool"
        license: MIT
        skip_upload: false          # true | false | "auto" (skip prereleases)
```

## Authentication

| Variable | Description |
|----------|-------------|
| `GITHUB_TOKEN` | Token with push access to your bucket repository |

The token can also be set via `repository.token` in the config.

## Common gotchas

- Only Windows archive artifacts are included in the manifest. Non-Windows targets are ignored.
- The `checkver` and `autoupdate` fields in the generated manifest reference the GitHub releases API, so the bucket can detect new versions automatically via `scoop update`.
- If the bucket repo requires a pull request (e.g., community buckets), use a fork + PR workflow — the direct-push model only works for self-hosted buckets.

## Generated manifest

The manifest includes:
- Download URL for the Windows archive
- SHA-256 checksum
- Binary extraction path
- `checkver` and `autoupdate` templates for automatic updates
