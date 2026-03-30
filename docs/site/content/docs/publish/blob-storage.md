+++
title = "Blob Storage"
description = "Upload release artifacts to S3, GCS, or Azure Blob Storage"
weight = 75
template = "docs.html"
+++

The blob storage stage uploads release artifacts to cloud object storage. It supports Amazon S3 (and compatible backends), Google Cloud Storage, and Azure Blob Storage.

## Minimal config

```yaml
crates:
  - name: myapp
    blobs:
      - provider: s3
        bucket: my-release-bucket
```

## Config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | string | | Unique identifier for referencing this config. |
| `provider` | string | **required** | Storage provider: `s3`, `gcs` (or `gs`), `azblob` (or `azure`). |
| `bucket` | string | **required** | Bucket or container name. Supports templates. |
| `directory` | string | `{{ ProjectName }}/{{ Tag }}` | Object key prefix within the bucket. Supports templates. |
| `region` | string | | AWS region (S3 only). Supports templates. |
| `endpoint` | string | | Custom endpoint URL for S3-compatible backends. Supports templates. |
| `disable_ssl` | bool | `false` | Disable TLS for the connection (S3 only). |
| `s3_force_path_style` | bool | `true` when `endpoint` set | Use path-style addressing instead of virtual-hosted style. Automatically enabled when `endpoint` is set. |
| `acl` | string | | Canned ACL for uploaded objects (e.g. `public-read`, `private`). |
| `cache_control` | string or list | | HTTP `Cache-Control` header. Accepts a single string or a list joined with `, `. |
| `content_disposition` | string | `attachment;filename={{Filename}}` | HTTP `Content-Disposition` header. Set to `-` to disable. Supports templates (includes `{{ Filename }}`). |
| `kms_key` | string | | AWS KMS key ARN for server-side encryption (S3 only). |
| `ids` | list | all | Filter to artifacts with these IDs. |
| `disable` | bool or template | `false` | Skip this blob config. Accepts a bool or a template string (e.g. `"{{ if IsSnapshot }}true{{ endif }}"`). |
| `include_meta` | bool | `false` | Also upload `metadata.json` and `artifacts.json`. |
| `extra_files` | list | | Additional files to upload. Supports glob patterns and optional name templates. |
| `extra_files_only` | bool | `false` | Upload only `extra_files`; skip all artifact uploads. |

### Extra files

Each entry under `extra_files` can have:

| Field | Description |
|-------|-------------|
| `glob` | Glob pattern for files to upload (required). |
| `name` / `name_template` | Override the upload filename. Supports templates including `{{ Filename }}`. |

```yaml
extra_files:
  - glob: dist/checksums.txt
  - glob: "release-notes/*.md"
    name: "release-notes-{{ Version }}.md"
```

## Authentication

Credentials are read from environment variables using each provider's standard chain.

### Amazon S3

| Variable | Description |
|----------|-------------|
| `AWS_ACCESS_KEY_ID` | Access key ID. |
| `AWS_SECRET_ACCESS_KEY` | Secret access key. |
| `AWS_SESSION_TOKEN` | Session token (for assumed roles). |
| `AWS_REGION` | Default region (overridden by `region` field). |
| `AWS_PROFILE` | Named profile from `~/.aws/credentials`. |

IAM instance profiles and ECS task roles are also supported automatically.

### Google Cloud Storage

| Variable | Description |
|----------|-------------|
| `GOOGLE_SERVICE_ACCOUNT` | Path to service account JSON key file. |
| `GOOGLE_SERVICE_ACCOUNT_PATH` | Alias for `GOOGLE_SERVICE_ACCOUNT`. |
| `GOOGLE_SERVICE_ACCOUNT_KEY` | JSON-serialized service account key (inline, not a file path). |

Application Default Credentials (ADC) via `gcloud auth application-default login` are also supported.

### Azure Blob Storage

| Variable | Description |
|----------|-------------|
| `AZURE_STORAGE_ACCOUNT_NAME` | Storage account name. |
| `AZURE_STORAGE_ACCOUNT_KEY` | Storage account key. |
| `AZURE_STORAGE_SAS_KEY` | Shared Access Signature token (alias: `AZURE_STORAGE_SAS_TOKEN`). |
| `AZURE_STORAGE_CONNECTION_STRING` | Full connection string. |
| `AZURE_CLIENT_ID` | Service principal client ID (with `AZURE_CLIENT_SECRET` and `AZURE_TENANT_ID`). |
| `AZURE_CLIENT_SECRET` | Service principal client secret. |
| `AZURE_TENANT_ID` | Azure AD tenant ID. |

## S3-compatible backends

Any S3-compatible service can be used by setting `endpoint`. When `endpoint` is set, `s3_force_path_style` defaults to `true` because most compatible services (MinIO, Cloudflare R2, DigitalOcean Spaces) require path-style addressing.

### MinIO

```yaml
blobs:
  - provider: s3
    bucket: my-bucket
    endpoint: http://minio.internal:9000
    region: us-east-1
```

### Cloudflare R2

```yaml
blobs:
  - provider: s3
    bucket: my-bucket
    endpoint: https://<account-id>.r2.cloudflarestorage.com
    region: auto
```

### DigitalOcean Spaces

```yaml
blobs:
  - provider: s3
    bucket: my-space
    endpoint: https://nyc3.digitaloceanspaces.com
    region: nyc3
```

## Full example

```yaml
crates:
  - name: myapp
    blobs:
      - provider: s3
        bucket: "my-releases-{{ ProjectName }}"
        directory: "{{ Version }}"
        region: us-east-1
        acl: public-read
        cache_control:
          - "public"
          - "max-age=31536000"
        kms_key: arn:aws:kms:us-east-1:123456789012:key/my-key-id
        include_meta: true
        extra_files:
          - glob: dist/checksums.txt
        disable: "{{ if IsSnapshot }}true{{ endif }}"
```

```yaml
crates:
  - name: myapp
    blobs:
      - provider: gcs
        bucket: my-gcs-bucket
        directory: "releases/{{ Tag }}"
        acl: publicRead

      - provider: azblob
        bucket: my-container
        directory: "{{ ProjectName }}/{{ Version }}"
```
