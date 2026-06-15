+++
title = "Metadata"
description = "Configure project metadata and control metadata.json output"
weight = 8
template = "docs.html"
+++

The `metadata` section sets project-level information used by publishers and made available as template variables.

## Minimal config

```yaml
metadata:
  description: "A fast CLI tool for data processing"
  homepage: "https://example.com/myapp"
  license: MIT
```

## Metadata config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `description` | string | none | Project description |
| `documentation` | string | Cargo `[package].documentation` | Project documentation URL. Drives the OCI `org.opencontainers.image.documentation` label and `{{ Metadata.Documentation }}`. Derived from `Cargo.toml` when unset. |
| `homepage` | string | none | Project homepage URL |
| `license` | string | Cargo `[package].license` | SPDX license identifier or expression (e.g., `MIT`, `Apache-2.0`, `MIT OR Apache-2.0`). Drives `{{ Metadata.License }}`. Derived from `Cargo.toml` when unset. |
| `maintainers` | list | none | List of maintainer names/emails |
| `mod_timestamp` | string | none | Template or Unix timestamp applied as mtime to metadata.json and artifacts.json |

## Template variables

All metadata fields are available as template variables:

| Variable | Description |
|----------|-------------|
| `{{ Metadata.Description }}` | Project description |
| `{{ Metadata.Documentation }}` | Documentation URL |
| `{{ Metadata.Homepage }}` | Homepage URL |
| `{{ Metadata.License }}` | SPDX license identifier |
| `{{ Metadata.Maintainers }}` | Maintainer list |
| `{{ Metadata.ModTimestamp }}` | Rendered mod_timestamp value |

## Reproducible timestamps

Use `mod_timestamp` to set a deterministic modification time on the generated metadata files:

```yaml
metadata:
  mod_timestamp: "{{ CommitTimestamp }}"
```

This renders the template at the end of the pipeline and applies the resulting Unix timestamp as the file mtime to both `metadata.json` and `artifacts.json`.

## Generated files

Anodizer always writes two JSON files to the dist directory:

- **`metadata.json`** — project metadata: `project_name`, `tag`, `previous_tag`, `version`, `commit`, `date`, and runtime info (`goos`, `goarch`)
- **`artifacts.json`** — full list of all artifacts produced during the release (see [Artifacts](/docs/general/artifacts/))

### Uploading metadata files

To include `metadata.json` and `artifacts.json` as release assets:

```yaml
release:
  include_meta: true
```

## Full example

```yaml
metadata:
  description: "A fast CLI tool for data processing"
  homepage: "https://example.com/myapp"
  license: Apache-2.0
  maintainers:
    - "Alice <alice@example.com>"
    - "Bob <bob@example.com>"
  mod_timestamp: "{{ CommitTimestamp }}"
```
