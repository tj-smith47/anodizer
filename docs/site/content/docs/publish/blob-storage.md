+++
title = "Blob Storage"
description = "Upload release artifacts to S3, GCS, or Azure Blob Storage"
weight = 75
template = "docs.html"
+++

The blob storage stage uploads release artifacts to cloud object storage. It supports Amazon S3 (and compatible backends), Google Cloud Storage, and Azure Blob Storage.

## Classification

| Group | Required (default) | Rollback | Token scope |
|---|---|---|---|
| Assets | false | delete each object written, including the ones a failed run wrote before it failed | backend credentials (see Authentication) |

See [Release resilience](../advanced/release-resilience.md) for the full classification table and the Submitter gate semantics.

## The `required:` field

Default: **`false`** — a blob storage upload failure is logged but does not fail the release.

Set `required: true` to make the release exit non-zero if this publisher fails:

```yaml
blobs:
  - provider: s3
    bucket: my-release-bucket
    required: true
```

See [Publish overview — the `required:` field](../) for the full semantics.

## Minimal config

```yaml
crates:
  - name: myapp
    blobs:
      - provider: s3
        bucket: my-release-bucket
```

## Full config reference

```yaml
blobs:
  - id: ""                                     # optional; unique identifier
    provider: s3                               # required; s3 | gcs (gs) | azblob (azure)
    bucket: "my-bucket"                        # required; supports templates
    directory: "{{ ProjectName }}/{{ Tag }}"   # optional; key prefix (template)
    region: us-east-1                          # optional; AWS region (template)
    endpoint: ""                               # optional; custom endpoint for S3-compatible backends
    disable_ssl: false                         # optional; disable TLS (S3 only)
    s3_force_path_style: false                 # optional; auto-true when endpoint is set
    acl: ""                                    # optional; e.g. public-read, private
    cache_control: ""                          # optional; string or list
    content_disposition: "attachment;filename={{Filename}}"  # optional; "-" to disable
    kms_key: ""                                # optional; AWS KMS key ARN (S3 only)
    ids: []                                    # optional; filter by artifact IDs
    exclude: []                                # optional; drop artifacts whose name matches a glob
    disable: false                             # optional; bool or template string
    include_meta: false                        # optional; also upload metadata.json
    extra_files: []                            # optional; additional files to upload
    extra_files_only: false                    # optional; skip artifacts, upload only extra_files
```

## Excluding sidecars with `exclude`

A heavy release attaches a checksum, a signature, and an SBOM next to every
archive — three sidecars per asset. When you mirror to a bucket you often want
the archives there but not the sidecars, both to save space and to stay under
provider rate limits. `exclude` is a list of globs matched against each
artifact's **file name** (e.g. `anodizer_0.12.4_x86_64.tar.gz`); anodizer drops
every artifact whose name matches at least one glob from **this blob target
only** — other destinations still receive the full set.

```yaml
blobs:
  - provider: s3
    bucket: my-mirror
    exclude:
      - "*.sha256"      # checksum sidecars
      - "*.sig"         # cosign / GPG signatures
      - "*.cdx.json"    # CycloneDX SBOMs
```

`exclude` composes with `ids:` — an artifact uploads only when it passes both
filters. An empty or unset `exclude` keeps everything. Globs are validated at
config-load, so a malformed pattern is rejected with a clear error rather than
silently keeping (or dropping) the wrong assets; if a non-empty candidate set
is reduced to zero, anodizer warns that the filter — not an empty release — is
why nothing uploaded.

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

## Common gotchas

- **`s3_force_path_style`** defaults to `true` when `endpoint` is set. Virtual-hosted style (`https://<bucket>.s3.amazonaws.com`) is only valid for AWS proper — S3-compatible backends (MinIO, R2, Spaces) require path style.
- **`disable` template**: the `disable` field accepts a template string, enabling conditional skip: `"{{ if IsSnapshot }}true{{ end }}"` skips blob upload for snapshot builds.
- **`content_disposition: "-"`**: set to the literal string `"-"` to disable the `Content-Disposition` header entirely (useful for direct-browser-download use cases).
- **Large files are streamed** in 10 MiB multipart parts, which changes the permissions an S3 identity needs and the object's ETag. See [How a file is uploaded](#how-a-file-is-uploaded).
- **A killed run can leave multipart parts behind.** They are invisible in a bucket listing and billed until removed. See [Incomplete multipart uploads](#incomplete-multipart-uploads).

## How a file is uploaded

A file is read from disk in fixed-size pieces while it uploads, so memory use does not grow with artifact size. One upload holds at most four parts in flight plus the part being filled, so it is bounded at (4 + 1) × 10 MiB plus a 1 MiB read buffer, 51 MiB, whatever the file's size. Files of one config upload in parallel (the config's `parallelism`, else the run's `--parallelism`), and each file in flight holds its own 51 MiB, so a config uploading four files at once can hold up to 204 MiB.

| File size | Requests | Result |
|---|---|---|
| under 10 MiB (10,485,760 bytes) | one PUT | an ordinary object |
| exactly 10 MiB | a multipart upload of one part | S3 ETag is a multipart ETag |
| over 10 MiB | a multipart upload in 10 MiB parts, four in flight (block uploads on Azure) | S3 ETag is a multipart ETag |

A multipart ETag is no MD5 of the file, so a tool that compares the two reports a mismatch for any object of 10 MiB or more.

`cache_control`, `content_disposition` and the detected `Content-Type` are set on the object on both paths. `acl` is sent as a header on every request to the bucket, the multipart ones included.

### S3 permissions

| Action | Needed for |
|---|---|
| `s3:PutObject` | the single PUT, and opening a multipart upload, uploading its parts and completing it |
| `s3:AbortMultipartUpload` | cleaning up after an upload that failed part way |
| `s3:GetObject` | the HEAD and ranged GETs of the comparison that skips a file already in the bucket with identical bytes |
| `s3:PutObjectAcl` | only when `acl` is set |
| `s3:DeleteObject` | rollback |
| `s3:ListBucketMultipartUploads` (on the bucket, not its objects) | optional; finding leftover uploads by hand (below) |
| `kms:GenerateDataKey` and `kms:Decrypt` on the key | only when the bucket or a plain `kms_key` encrypts with SSE-KMS: S3 decrypts the parts of a multipart upload to assemble them, and the comparison reads the existing object |
| `kms:Encrypt` on the key | only for an `awskms://` `kms_key` ([client-side encryption](#client-side-encryption)) |

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": [
        "s3:PutObject",
        "s3:PutObjectAcl",
        "s3:GetObject",
        "s3:DeleteObject",
        "s3:AbortMultipartUpload"
      ],
      "Resource": "arn:aws:s3:::my-release-bucket/*"
    }
  ]
}
```

Without `s3:GetObject` the comparison cannot read the bucket, and every run uploads every file again.

### A file already in the bucket

Before uploading, anodizer compares the file with the object at its key, 10 MiB at a time, and skips the upload when every byte matches. An object that differs, that changes while it is being compared, or that cannot be read is uploaded over.

### A run that fails part way

Files are uploaded independently, so a run can write some objects and fail on another. The run report records the ones it wrote on the failed `blob` row:

```text
$ anodizer release
     • uploaded 2 object(s), skipped 0 (identical) → s3://my-release-bucket/releases/v1.0.0
     • blob upload failed: blobs: the multipart upload was aborted or expired part way through (releases/v1.0.0/app.tar.gz): uploading dist/app.tar.gz → releases/v1.0.0/app.tar.gz; run the release again to upload the file
```

Two ways forward, both safe:

| Command | What happens to the two objects already written |
|---|---|
| the same `anodizer release` again | skipped as identical; only the missing file is uploaded |
| `anodizer tag rollback` | deleted, because the failed row names them |

A rollback after a successful re-run deletes all three: the re-run's report takes over the objects the failed run recorded, as long as `dist/run-<tag>/` is still there for the second run (`--clean` keeps it; a fresh checkout does not have it). See [What a failed publisher leaves on record](../advanced/release-resilience.md#what-a-failed-publisher-leaves-on-record).

### What a rollback deletes

The rollback deletes the objects this release created. An object that already existed at its key before the release and was uploaded over is left in place: deleting it would not bring the previous version back. Each such object is named in a warning, and the summary line counts it:

```text
$ anodizer tag rollback
   • deleted s3://my-release-bucket/releases/v1.0.0/app.tar.gz
     Warning s3://my-release-bucket/releases/v1.0.0/checksums.txt overwrote an object that existed before this release; left in place (restore the previous version from the bucket's own versioning or a backup)
   • blob rollback complete — 1 deleted, 0 already absent, 1 overwritten (left in place), 0 failed
```

The `blob_targets` entries of the run report carry `"overwrote": true` on those objects, so a rollback run later from the same report makes the same decision.

### Content-Type

Every object gets a `Content-Type` detected from its first 512 bytes; the file name plays no part.

| File | `Content-Type` |
|---|---|
| `.tar.gz`, `.tgz`, `.apk` | `application/x-gzip` |
| `.zip` | `application/zip` |
| `.pdf` | `application/pdf` |
| `.tar`, `.tar.xz`, `.tar.zst`, `.deb`, `.rpm`, `.exe`, `.msi`, a raw binary, a binary signature | `application/octet-stream` |
| checksums, `.json`, `.pem`, an armored or base64 signature, an empty file | `text/plain; charset=utf-8` |

```bash
$ aws s3api head-object --bucket my-release-bucket --key myapp/v1.0.0/myapp_1.0.0_linux_amd64.tar.gz --query ContentType
"application/x-gzip"
```

A client-side encrypted file (below) is typed from its ciphertext, which is what the object holds.

### Incomplete multipart uploads

An upload that fails part way is aborted: no object appears at the key, and an object already there keeps its old bytes. The parts sent so far are a separate thing. The abort removes them, but a run that is killed, loses its connection, or whose abort request fails or gets no answer within 30 seconds leaves them stored. They do not show in a bucket listing and are billed until removed. When anodizer knows an abort did not go through, it prints a warning naming the object and, on S3, the two commands that remove the upload:

```text
     Warning an incomplete multipart upload may remain at s3://my-release-bucket/myapp/v1.0.0/myapp.tar.gz (aborting it got no answer within 30s); its parts are stored and billed until it is aborted — find its upload ID with `aws s3api list-multipart-uploads --bucket 'my-release-bucket' --prefix 'myapp/v1.0.0/myapp.tar.gz'` and remove it with `aws s3api abort-multipart-upload --bucket 'my-release-bucket' --key 'myapp/v1.0.0/myapp.tar.gz' --upload-id <UploadId>`, or set an AbortIncompleteMultipartUpload lifecycle rule on the bucket
```

With a custom `endpoint`, both commands carry `--endpoint-url`.

On S3, set a lifecycle rule on the bucket so the provider removes them:

```json
{
  "Rules": [
    {
      "ID": "abort-incomplete-multipart-uploads",
      "Status": "Enabled",
      "Filter": { "Prefix": "" },
      "AbortIncompleteMultipartUpload": { "DaysAfterInitiation": 1 }
    }
  ]
}
```

```bash
aws s3api put-bucket-lifecycle-configuration --bucket my-release-bucket --lifecycle-configuration file://lifecycle.json
```

Or remove one by hand:

```bash
aws s3api list-multipart-uploads --bucket my-release-bucket
aws s3api abort-multipart-upload --bucket my-release-bucket --key <key> --upload-id <id>
```

Google Cloud Storage has a lifecycle action of the same name, written in its own format and limited to the `age`, `matchesPrefix` and `matchesSuffix` conditions. `gcloud` has no command that lists or aborts a multipart upload, so the rule is the cleanup:

```json
{
  "lifecycle": {
    "rule": [
      {
        "action": { "type": "AbortIncompleteMultipartUpload" },
        "condition": { "age": 1 }
      }
    ]
  }
}
```

```bash
gcloud storage buckets update gs://my-gcs-bucket --lifecycle-file=lifecycle.json
```

Azure has nothing to abort and no rule to set: blocks that were never committed are removed a week after the last block was written to the blob, and sooner when the file is uploaded again. A failed upload there says so once per file, at the same level as the other providers' warning:

```text
     Warning uncommitted blocks may remain at azblob://my-container/myapp/v1.0.0/myapp.tar.gz (Azure has no abort that removes blocks already staged); Azure removes them a week after the last block was written, and uploading the file again removes them sooner
```

### Client-side encryption

A `kms_key` written as a URL encrypts each file on the runner before upload, through the provider's CLI. Such a file is read whole, and the encryption call limits its size. A file over the limit fails the stage before anything is uploaded.

| `kms_key` | Provider | CLI | Largest file |
|---|---|---|---|
| `awskms://…` | `s3` | `aws` | 4,096 bytes |
| `gcpkms://…` | `gcs` | `gcloud` | 65,536 bytes with a software or external key, 8,192 with an HSM-protected key |
| `azurekeyvault://…` | `azblob` | `az` | 446 bytes with a 4096-bit RSA key (318 with 3072-bit, 190 with 2048-bit); the algorithm is `RSA-OAEP-256` |

```yaml
blobs:
  - provider: gcs
    bucket: my-gcs-bucket
    kms_key: gcpkms://projects/p/locations/global/keyRings/r/cryptoKeys/k
    extra_files_only: true          # small files only
    extra_files:
      - glob: dist/checksums.txt
```

anodizer enforces the larger ceiling of each provider itself. Under a smaller Azure key or an HSM-protected Cloud KMS key the provider refuses the file, and the error names the smaller limit. An Azure key is written `azurekeyvault://<vault>/keys/<name>`, with an optional `/<version>` after the name. A plain key ARN or ID (no URL scheme) is server-side encryption on S3 and has no size limit.

Each component of the URL is handed to the provider's CLI as a command-line argument, so a component the CLI would read as something else is refused before any CLI runs: one starting with `-` (an option to every CLI), and on AWS one starting with `file://`, `fileb://`, `http://` or `https://`, which `aws` would dereference as a file or URL to load the key id from.

`aws` and `gcloud` read the plaintext from stdin. `az keyvault key encrypt` has no file or stdin form for its input: the plaintext is passed base64-encoded on the command line as `--value`, where it is visible to anyone on the runner who can list processes (`ps`) for as long as the `az` call runs. Use Azure client-side encryption only on a runner whose processes nobody else can list.

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
| `kms_key` | string | | A plain AWS KMS key ARN or ID: server-side encryption (S3 only). An `awskms://`, `gcpkms://` or `azurekeyvault://` URL: [client-side encryption](#client-side-encryption), with a per-provider file size limit. |
| `ids` | list | all | Filter to artifacts with these IDs. |
| `disable` | bool or template | `false` | Skip this blob config. Accepts a bool or a template string (e.g. `"{{ if IsSnapshot }}true{{ end }}"`). |
| `include_meta` | bool | `false` | Also upload `metadata.json`. The sibling `dist/artifacts.json` manifest is never uploaded. |
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
        skip: "{{ if IsSnapshot }}true{{ end }}"
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
