+++
title = "Artifactory"
description = "Upload artifacts to JFrog Artifactory"
weight = 80
template = "docs.html"
+++

Anodizer can upload release artifacts to JFrog Artifactory repositories.

## Classification

| Group | Required (default) | Rollback | Token scope |
|---|---|---|---|
| Assets | false | parallel HTTP DELETE per uploaded URL (404/410 treated as already-absent) | `ARTIFACTORY_{NAME}_SECRET` delete |

See [Release resilience](../advanced/release-resilience.md) for the full classification table and the Submitter gate semantics.

## The `required:` field

Default: **`false`** — an Artifactory upload failure is logged but does not fail the release.

Set `required: true` to make the release exit non-zero if this publisher fails:

```yaml
artifactories:
  - name: production
    target: "https://artifactory.example.com/repo/path/"
    required: true
```

See [Publish overview — the `required:` field](../) for the full semantics.

## Minimal config

```yaml
artifactories:
  - name: production
    target: "https://artifactory.example.com/repo/path/"
```

## Artifactory config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | **required** | Identifier used for env var lookup |
| `target` | string | **required** | Upload URL (template, artifact-specific vars available) |
| `mode` | string | `archive` | Artifact selection: `"archive"` or `"binary"` |
| `username` | string | env fallback | HTTP basic auth username |
| `password` | string | env fallback | HTTP basic auth password |
| `ids` | list | none | Filter by build IDs |
| `exts` | list | none | Filter by file extensions |
| `method` | string | `PUT` | HTTP method (`PUT` or `POST`) |
| `checksum_header` | string | `X-Checksum-SHA256` | Header name for SHA-256 checksum |
| `custom_headers` | map | none | Extra HTTP headers (template-rendered) |
| `checksum` | bool | `false` | Include checksum files |
| `signature` | bool | `false` | Include signature files |
| `meta` | bool | `false` | Include metadata.json and artifacts.json |
| `custom_artifact_name` | bool | `false` | Use artifact name as-is (don't append to target URL) |
| `extra_files` | list | none | Additional files to upload |
| `extra_files_only` | bool | `false` | Only upload extra files, skip artifacts |
| `client_x509_cert` | string | none | Path to client TLS certificate |
| `client_x509_key` | string | none | Path to client TLS private key |
| `trusted_certificates` | string | none | Path to CA certificate bundle |
| `skip` | string/bool | none | Skip this config |

## Full config reference

```yaml
artifactories:
  - name: production          # required; sets ARTIFACTORY_{NAME}_SECRET env lookup
    target: "https://artifactory.example.com/repo/{{ .Version }}/{{ .ArtifactName }}"
    mode: archive             # archive | binary
    method: PUT               # PUT | POST
    username: ""              # falls back to ARTIFACTORY_{NAME}_USERNAME
    password: ""              # falls back to ARTIFACTORY_{NAME}_SECRET
    ids: []
    exts: []
    checksum_header: "X-Checksum-SHA256"
    custom_headers: {}        # template-rendered
    checksum: false
    signature: false
    meta: false
    custom_artifact_name: false
    extra_files: []
    extra_files_only: false
    client_x509_cert: ""
    client_x509_key: ""
    trusted_certificates: ""
    skip: false
```

## Authentication

Credentials are resolved in this order:

| Variable | Fallback |
|----------|----------|
| Username | config value, then `ARTIFACTORY_{NAME}_USERNAME` |
| Password | `ARTIFACTORY_{NAME}_SECRET`, then `ARTIFACTORY_SECRET`, then config value |

Where `{NAME}` is the uppercased `name` field.

## Common gotchas

- **`PUT` vs `POST`**: the default method is `PUT`. Some Artifactory configurations require `POST` for initial upload and reject `PUT` with a 405. Set `method: POST` if uploads fail with a 405.
- **Credential resolution order**: username and password are tried in config value → env var order (see Authentication above). An empty config value falls through to the env var.
- **`custom_artifact_name: true`**: uses the artifact filename as-is instead of appending it to the `target` URL. Use this when `target` already includes the full artifact path.

## Target URL templating

The `target` URL and `custom_headers` values support artifact-specific template variables:

| Variable | Description |
|----------|-------------|
| `{{ .ArtifactName }}` | Artifact filename |
| `{{ .ArtifactExt }}` | File extension |
| `{{ .Os }}` | Target OS |
| `{{ .Arch }}` | Target architecture |
| `{{ .Target }}` | Rust target triple |

## Full example

```yaml
artifactories:
  - name: production
    target: "https://artifactory.example.com/myapp/{{ .Version }}/{{ .ArtifactName }}"
    mode: archive
    custom_headers:
      X-Build-Number: "{{ .Env.BUILD_NUMBER }}"
    checksum: true
    signature: true
```
