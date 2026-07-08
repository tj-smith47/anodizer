+++
title = "GitHub Releases"
description = "Create GitHub releases with uploaded assets"
weight = 1
template = "docs.html"
+++

The release stage creates a GitHub release and uploads all artifacts as assets.

## Classification

| Group | Required (default) | Rollback | Token scope |
|---|---|---|---|
| Assets | true | delete release + delete assets (tag ref owned by `tag rollback`) | `contents:write` |

See [Release resilience](../advanced/release-resilience.md) for the full classification table and the Submitter gate semantics.

## The `required:` field

Default: **`true`** — a GitHub Releases failure fails the release.

Set `required: false` to log failures but continue:

```yaml
crates:
  - name: myapp
    release:
      required: false   # continue release even if GitHub Release upload fails
      github:
        owner: myorg
        name: myapp
```

See [Publish overview — the `required:` field](../) for the full semantics.

## Minimal config

```yaml
crates:
  - name: myapp
    release:
      github:
        owner: myorg
        name: myapp
```

If `github.owner` and `github.name` are omitted, anodizer auto-detects them from the git remote URL.

## Release config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `github.owner` | string | auto-detected | GitHub owner/org |
| `github.name` | string | auto-detected | Repository name |
| `draft` | bool | `false` | Create as draft release |
| `prerelease` | string/bool | `auto` | Mark as prerelease: `auto` (detect from version), `true`, `false` |
| `make_latest` | string/bool | `auto` | Mark as latest: `auto`, `true`, `false` |
| `name_template` | string | `{{ Tag }}` | Release title |
| `header` | string | none | Text prepended to release body |
| `footer` | string | none | Text appended to release body |
| `extra_files` | list | none | Additional files to upload (glob patterns) |
| `skip_upload` | bool | `false` | Create release without uploading assets |
| `replace_existing_draft` | bool | `false` | Replace existing draft release. See [Recovery flags](../advanced/recovery-flags.md#release-replace-existing-draft). |
| `replace_existing_artifacts` | bool | `false` | Overwrite existing assets. See [Recovery flags](../advanced/recovery-flags.md#release-replace-existing-artifacts). |
| `on_failure` | string | `rollback` | In-process failure policy: `rollback` or `hold`. See [Release resilience](../advanced/release-resilience.md#release-on-failure-the-in-process-failure-policy). |

## Full config reference

```yaml
release:
  github:
    owner: myorg              # auto-detected from git remote if omitted
    name: myapp
  draft: false
  prerelease: auto            # auto | true | false
  make_latest: auto           # auto | true | false
  name_template: "{{ Tag }}"
  header: ""                  # text prepended to release body
  footer: ""                  # text appended to release body
  extra_files: []             # glob patterns for additional uploads
  skip_upload: false          # bool or template string
  replace_existing_draft: false
  replace_existing_artifacts: false
  mode: ""                    # keep-existing | append | prepend | replace
  ids: []
  exclude: []                 # drop assets whose name matches a glob
  skip: false
  on_failure: rollback        # rollback | hold (auto-degrades to hold past one-way doors)
```

## Excluding sidecars with `exclude`

`exclude` is a list of globs matched against each release asset's **file
name**; anodizer drops every asset whose name matches at least one glob before
attaching it to **this GitHub release only** (a mirror configured elsewhere is
unaffected). Use it to keep heavy sidecars (checksums, signatures, SBOMs) off
the GitHub release while archives still attach.

```yaml
release:
  github: { owner: my-org, name: my-repo }
  exclude:
    - "*.sha256"
    - "*.sig"
    - "*.cdx.json"
```

`exclude` composes with `ids:` — an asset attaches only when it passes both
filters. An empty or unset `exclude` keeps everything. Globs are validated at
config-load; an `exclude` that drops every candidate raises a warning so a
typo'd glob is never a silent empty release. The `verify-release` gate applies
the same `exclude`, so a deliberately-excluded signature or SBOM is not flagged
as a missing asset.

## Authentication

Set `GITHUB_TOKEN` as an environment variable or pass it via `--token`:

```bash
export GITHUB_TOKEN="ghp_..."
anodizer release
```

## Common gotchas

- **Draft vs published**: a draft release is only visible to repo collaborators. Set `draft: false` (the default) for publicly visible releases.
- **`make_latest: auto`**: by default, anodizer marks a release as latest only when the version is not a prerelease. Override with `make_latest: true` or `make_latest: false`.
- **Asset name collisions**: if two artifacts have the same filename, the second upload returns a 422. Set `replace_existing_artifacts: true` on the `release:` block to overwrite.

## Republish / update behavior

Use `replace_existing_draft: true` and `replace_existing_artifacts: true` on the `release:` block for re-runnable workflows. See [Recovery flags](../advanced/recovery-flags.md#release-replace-existing-draft) for the full mechanism.

## Draft releases

```yaml
release:
  draft: true
```

## Prerelease detection

When `prerelease: auto` (default), anodizer detects prereleases from the version string. Versions like `1.0.0-rc.1`, `1.0.0-beta.2` are automatically marked as prereleases.

## Extra files

Upload additional files that aren't part of the pipeline:

```yaml
release:
  extra_files:
    - "dist/completions/*"
    - "docs/man/*.1"
```

## Full example

```yaml
crates:
  - name: myapp
    release:
      github:
        owner: myorg
        name: myapp
      name_template: "{{ ProjectName }} {{ Version }}"
      header: |
        ## What's Changed
      footer: |
        **Full Changelog**: https://github.com/myorg/myapp/compare/{{ PreviousTag }}...{{ Tag }}
      prerelease: auto
      make_latest: auto
```
