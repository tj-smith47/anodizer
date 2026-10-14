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

## The `required:` field

Default: **`false`** — a Scoop bucket push failure is logged but does not fail the release.

Set `required: true` to make the release exit non-zero if this publisher fails:

```yaml
crates:
  - name: myapp
    publish:
      scoop:
        repository:
          owner: myorg
          name: scoop-bucket
        required: true
```

See [Publish overview — the `required:` field](../) for the full semantics.

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
| `repository.pull_request.token` | string | `repository.token`, then `GITHUB_TOKEN` | Token used to open the PR when `repository.pull_request.enabled` is set (templates allowed). Set it when the bucket token cannot open pull requests. |
| `description` | string | Cargo `[package].description` | Manifest description. Derived from `Cargo.toml`; set to override. |
| `directory` | string | `bucket` | Subdirectory in the bucket repo for manifest placement. Scoop resolves manifests only from `bucket/` when that directory exists (repo root otherwise), so `bucket/` is correct for both layouts. Set to `""` to target the repo root. A stale same-named root-level manifest is removed when publishing into a subdirectory. |
| `license` | string | Cargo `[package].license` | License identifier. Derived from `Cargo.toml`; set to override. |
| `checkver` | string | `github` | Version-detection strategy emitted into the manifest. Defaults to `github` (derived from the GitHub repo); override with a homepage regex (e.g. `v([\d.]+)`) when GitHub release detection is not appropriate. |

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
          branch: ""                # default: repo default branch (versioned <manifest>-<version> when pull_request.enabled)
          pull_request:
            enabled: false          # optional; open PR instead of direct push
            token: ""               # token used to open the PR (default: repository.token, then GITHUB_TOKEN)
        description: "A fast CLI tool"   # optional; derived from Cargo.toml description
        license: MIT                     # optional; derived from Cargo.toml license
        directory: bucket                # optional; default "bucket" ("" targets the repo root)
        checkver: github                 # optional; "github" (default) or a homepage regex
        skip_upload: false          # true | false | "auto" (skip prereleases)
```

## Authentication

| Variable | Description |
|----------|-------------|
| `GITHUB_TOKEN` | Token with push access to your bucket repository |

The token can also be set via `repository.token` in the config.

`repository.pull_request.token` and `repository.token` are rendered before anything is cloned or pushed. A blank render, such as an unset `{{ .Env.X }}`, falls through to the next token in the chain. A template that fails to render skips this crate's scoop entry with nothing pushed, and the next crate still publishes. The skip reason names the field and the render error (`pull_request.token does not render: …`), with the template's own text replaced by `<template>` so a token literal in it is never printed.

## Common gotchas

- Only Windows archive artifacts are included in the manifest. Non-Windows targets are ignored.
- The `checkver` and `autoupdate` fields in the generated manifest reference the GitHub releases API, so the bucket can detect new versions automatically via `scoop update`.
- If the bucket repo requires a pull request (e.g., community buckets), use a fork + PR workflow — the direct-push model only works for self-hosted buckets.

## Auto-update: `checkver`, `autoupdate`, per-arch `extract_dir`

`ScoopInstaller/Main` (and most community buckets) require a `checkver` +
`autoupdate` pair so the bucket can detect and fetch new releases. Anodizer
derives both automatically — you do not hand-write them:

- **`checkver`** defaults to `github`, pointing the bucket at the GitHub
  releases API for version detection. Override it with a homepage regex only
  when GitHub detection doesn't fit.
- **`autoupdate`** is emitted as a per-architecture block whose `url` is your
  archive `name_template` with the concrete version replaced by scoop's
  `$version` placeholder, plus a `hash` rule wired to the release's
  checksums. A `checkver` is only emitted alongside a usable `autoupdate`
  block — a `checkver` without `autoupdate` is a dead half-manifest, so
  anodizer never emits one alone.
- **`extract_dir`** is set per-architecture only when the archive wraps its
  contents in a top-level directory; both the live `architecture` block and the
  `autoupdate` block carry the matching `extract_dir` (with `$version`
  substituted in the autoupdate copy) so `scoop install` and `scoop update`
  find the binary at the same path.

```json
"checkver": "github",
"autoupdate": {
  "architecture": {
    "64bit": {
      "url": "https://github.com/myorg/myapp/releases/download/v$version/myapp-$version-windows-amd64.zip"
    }
  }
}
```

## Generated manifest

The manifest includes:
- Download URL for the Windows archive
- SHA-256 checksum
- Binary extraction path (and a per-arch `extract_dir` when the archive nests its contents)
- `checkver` and `autoupdate` templates for automatic updates
