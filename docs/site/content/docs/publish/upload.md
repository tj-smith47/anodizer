+++
title = "Upload"
description = "Upload artifacts to any HTTP server"
weight = 87
template = "docs.html"
+++

The generic upload publisher lets you upload artifacts to any HTTP server. It works the same way as the [Artifactory](/docs/publish/artifactory/) publisher but with different environment variable naming.

## Classification

| Group | Required (default) | Rollback | Token scope |
|---|---|---|---|
| Assets | false | warn-only (no standard HTTP DELETE; implement rollback via `after:` hooks if needed) | `UPLOAD_{NAME}_SECRET` or basic auth |

See [Release resilience](../advanced/release-resilience.md) for the full classification table and the Submitter gate semantics.

## Minimal config

```yaml
uploads:
  - name: myserver
    target: "https://files.example.com/releases/{{ Version }}/"
```

## Full config reference

```yaml
uploads:
  - name: upload                       # optional; identifier used for env-var lookup
    target: "https://files.example.com/releases/{{ Version }}/"  # required (template)
    mode: archive                      # optional; archive | binary
    method: PUT                        # optional; PUT | POST
    username: ""                       # optional; falls back to UPLOAD_{NAME}_USERNAME
    password: ""                       # optional; falls back to UPLOAD_{NAME}_SECRET
    ids: []                            # optional; filter by build IDs
    exts: []                           # optional; filter by file extensions
    checksum_header: ""                # optional; HTTP header name for SHA-256
    custom_headers: {}                 # optional; extra HTTP headers (template-rendered)
    checksum: false                    # optional; also upload checksum files
    signature: false                   # optional; also upload signature files
    meta: false                        # optional; also upload metadata.json + artifacts.json
    custom_artifact_name: false        # optional; do not append artifact name to target URL
    extra_files: []                    # optional; additional files to upload
    extra_files_only: false            # optional; skip artifact uploads
    client_x509_cert: ""               # optional; client TLS cert path
    client_x509_key: ""                # optional; client TLS private key path
    trusted_certificates: ""           # optional; CA bundle path
    skip: false                        # optional
```

## Authentication

| Variable | Fallback |
|----------|----------|
| Username | config value, then `UPLOAD_{NAME}_USERNAME` |
| Password | `UPLOAD_{NAME}_SECRET`, then config value |

Where `{NAME}` is the uppercased `name` field.

## Common gotchas

- Same caveats as Artifactory: `PUT` is the default method; some servers require `POST`. Set `method: POST` if uploads fail with a 405.
- `custom_artifact_name: true` uses the artifact filename as-is instead of appending it to the `target` URL.
- No programmatic rollback — the upload publisher does not attempt HTTP DELETE on rollback. Use `after:` hooks for custom cleanup if needed.

## Upload config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | `upload` | Identifier used for env var lookup |
| `target` | string | **required** | Upload URL (template, artifact-specific vars available) |
| `mode` | string | `archive` | Artifact selection: `"archive"` or `"binary"` |
| `method` | string | `PUT` | HTTP method (`PUT` or `POST`) |
| `username` | string | env fallback | HTTP basic auth username |
| `password` | string | env fallback | HTTP basic auth password |
| `ids` | list | none | Filter by build IDs |
| `exts` | list | none | Filter by file extensions |
| `checksum_header` | string | `""` | Header name for SHA-256 checksum |
| `custom_headers` | map | none | Extra HTTP headers (template-rendered) |
| `checksum` | bool | `false` | Include checksum files |
| `signature` | bool | `false` | Include signature files |
| `meta` | bool | `false` | Include metadata.json and artifacts.json |
| `custom_artifact_name` | bool | `false` | Use artifact name as-is (don't append to target URL) |
| `extra_files` | list | none | Additional files to upload |
| `extra_files_only` | bool | `false` | Only upload extra files |
| `client_x509_cert` | string | none | Path to client TLS certificate |
| `client_x509_key` | string | none | Path to client TLS private key |
| `trusted_certificates` | string | none | Path to CA certificate bundle |
| `skip` | string/bool | none | Skip this config (template-conditional) |

## Target URL templating

The `target` URL supports artifact-specific template variables:

| Variable | Description |
|----------|-------------|
| `{{ ArtifactName }}` | Artifact filename |
| `{{ ArtifactExt }}` | File extension |
| `{{ Os }}` | Target OS |
| `{{ Arch }}` | Target architecture |
| `{{ Target }}` | Rust target triple |

When `custom_artifact_name` is `false` (default), the artifact filename is automatically appended to the target URL.

## Full example

```yaml
uploads:
  - name: releases
    target: "https://releases.example.com/{{ ProjectName }}/{{ Version }}/"
    mode: archive
    checksum: true
    custom_headers:
      X-Deploy-Token: "{{ Env.DEPLOY_TOKEN }}"
```
