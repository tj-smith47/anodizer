+++
title = "GitHub Releases"
description = "Create GitHub releases with uploaded assets"
weight = 1
template = "docs.html"
+++

The release stage creates a GitHub release and uploads all artifacts as assets.

## Minimal config

```yaml
crates:
  - name: myapp
    release:
      github:
        owner: myorg
        name: myapp
```

If `github.owner` and `github.name` are omitted, anodize auto-detects them from the git remote URL.

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
| `replace_existing_draft` | bool | `false` | Replace existing draft release |
| `replace_existing_artifacts` | bool | `false` | Overwrite existing assets |

## Authentication

Set `GITHUB_TOKEN` as an environment variable or pass it via `--token`:

```bash
export GITHUB_TOKEN="ghp_..."
anodize release
```

## Draft releases

```yaml
release:
  draft: true
```

## Prerelease detection

When `prerelease: auto` (default), anodize detects prereleases from the version string. Versions like `1.0.0-rc.1`, `1.0.0-beta.2` are automatically marked as prereleases.

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
