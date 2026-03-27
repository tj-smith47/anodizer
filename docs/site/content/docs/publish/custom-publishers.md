+++
title = "Custom Publishers"
description = "Run arbitrary commands on release artifacts"
weight = 11
template = "docs.html"
+++

Custom publishers let you run any command on your release artifacts, enabling integration with tools and registries that anodize doesn't natively support.

## Minimal config

```yaml
publishers:
  - name: my-publisher
    cmd: ./scripts/publish.sh
    args: ["{{ ArtifactPath }}", "{{ Version }}"]
```

## Publisher config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | — | Publisher name (for logging) |
| `cmd` | string | — | Command to execute |
| `args` | list | none | Arguments (templates supported) |
| `ids` | list | none | Only run on artifacts matching these IDs |
| `artifact_types` | list | none | Filter by type: `binary`, `archive`, `checksum`, `package` |
| `env` | map | none | Additional environment variables |

## Filtering artifacts

```yaml
publishers:
  - name: upload-binaries
    cmd: ./scripts/upload.sh
    artifact_types: [binary]
    args: ["{{ ArtifactPath }}"]
```
